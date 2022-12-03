/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::collections::HashMap;
use std::fmt::Debug;
use std::mem;
use std::sync::Arc;

use allocative::Allocative;
use anyhow::Context as _;
use async_trait::async_trait;
use buck2_common::result::SharedResult;
use buck2_core::cells::paths::CellRelativePath;
use buck2_core::cells::CellName;
use buck2_core::collections::ordered_map::OrderedMap;
use buck2_core::configuration::transition::applied::TransitionApplied;
use buck2_core::configuration::transition::id::TransitionId;
use buck2_core::configuration::Configuration;
use buck2_core::configuration::ConfigurationData;
use buck2_core::package::Package;
use buck2_core::pattern::lex_target_pattern;
use buck2_core::pattern::ParsedPattern;
use buck2_core::pattern::PatternData;
use buck2_core::pattern::TargetPattern;
use buck2_core::provider::label::ConfiguredProvidersLabel;
use buck2_core::provider::label::ProvidersLabel;
use buck2_core::provider::label::ProvidersName;
use buck2_core::target::TargetLabel;
use buck2_core::target::TargetName;
use buck2_core::unsafe_send_future::UnsafeSendFuture;
use buck2_execute::anon_target::AnonTarget;
use buck2_execute::base_deferred_key::BaseDeferredKey;
use buck2_interpreter::starlark_promise::StarlarkPromise;
use buck2_interpreter::types::label::Label;
use buck2_interpreter_for_build::attrs::coerce::attr_type::AttrTypeInnerExt;
use buck2_node::attrs::attr::Attribute;
use buck2_node::attrs::coerced_path::CoercedPath;
use buck2_node::attrs::coercion_context::AttrCoercionContext;
use buck2_node::attrs::configurable::AttrIsConfigurable;
use buck2_node::attrs::configuration_context::AttrConfigurationContext;
use buck2_node::attrs::configured_attr::ConfiguredAttr;
use buck2_node::configuration::execution::ExecutionPlatformResolution;
use derive_more::Display;
use dice::DiceComputations;
use dice::Key;
use futures::future;
use futures::Future;
use gazebo::prelude::*;
use ref_cast::RefCast;
use starlark::collections::SmallMap;
use starlark::environment::Module;
use starlark::eval::Evaluator;
use starlark::values::dict::DictOf;
use starlark::values::structs::Struct;
use starlark::values::Trace;
use starlark::values::Value;
use starlark::values::ValueTyped;
use thiserror::Error;

use crate::analysis::calculation::get_rule_impl;
use crate::analysis::registry::AnalysisRegistry;
use crate::analysis::AnalysisResult;
use crate::analysis::RuleAnalysisAttrResolutionContext;
use crate::analysis::RuleImplFunction;
use crate::attrs::resolve::configured_attr::ConfiguredAttrExt;
use crate::deferred::types::DeferredTable;
use crate::interpreter::rule_defs::context::AnalysisContext;
use crate::interpreter::rule_defs::provider::collection::FrozenProviderCollectionValue;
use crate::interpreter::rule_defs::provider::collection::ProviderCollection;
use crate::interpreter::rule_defs::rule::FrozenRuleCallable;
use crate::nodes::calculation::find_execution_platform_by_configuration;

#[derive(Debug, Trace, Allocative)]
pub(crate) struct AnonTargetsRegistry<'v> {
    // We inherit the execution platform of our parent
    execution_platform: ExecutionPlatformResolution,
    // The actual data
    entries: Vec<(ValueTyped<'v, StarlarkPromise<'v>>, AnonTargetKey)>,
}

#[derive(Debug, Error)]
enum AnonTargetsError {
    #[error("Not allowed to call `anon_targets` in this context")]
    AssertNoPromisesFailed,
    #[error(
        "Invalid `name` attribute, must be a label or a string, got `{value}` of type `{typ}`"
    )]
    InvalidNameType { typ: String, value: String },
    #[error("`name` attribute must be a valid target label, got `{0}`")]
    NotTargetLabel(String),
    #[error("can't parse strings during `anon_targets` coercion, got `{0}`")]
    CantParseDuringCoerce(String),
    #[error("Unknown attribute `{0}`")]
    UnknownAttribute(String),
}

#[repr(transparent)]
#[derive(
    Hash, Eq, PartialEq, Clone, Dupe, Debug, Display, Trace, Allocative, RefCast
)]
#[display(fmt = "{:?}", self)]
struct AnonTargetKey(Arc<AnonTarget>);

impl AnonTargetKey {
    fn new<'v>(
        execution_platform: &ExecutionPlatformResolution,
        rule: ValueTyped<'v, FrozenRuleCallable>,
        attributes: DictOf<'v, &'v str, Value<'v>>,
    ) -> anyhow::Result<Self> {
        let mut name = None;

        let entries = attributes.collect_entries();
        let attrs_spec = rule.attributes();
        // The capacity might be over by one, if `name` is an entry, but small over is not a big deal
        let mut attrs = OrderedMap::with_capacity(entries.len());
        for (k, v) in entries {
            if k == "name" {
                name = Some(Self::coerce_name(v)?);
            } else {
                let attr = attrs_spec
                    .attribute(k)
                    .ok_or_else(|| AnonTargetsError::UnknownAttribute(k.to_owned()))?;
                attrs.insert(
                    k.to_owned(),
                    Self::coerce_attr(attr, v)
                        .with_context(|| format!("when coercing attribute `{}`", k))?,
                );
            }
        }
        // We need to ensure there is a "name" attribute which corresponds to something we can turn in to a label.
        // If there isn't a good one, make something up
        let name = match name {
            None => Self::create_name(&rule.rule_type().name)?,
            Some(name) => name,
        };

        Ok(Self(Arc::new(AnonTarget::new(
            rule.rule_type().dupe(),
            name,
            attrs.into(),
            execution_platform.cfg(),
        ))))
    }

    /// We need to parse a TargetLabel from a String, but it doesn't matter if the pieces aren't
    /// valid targets in the context of this build (e.g. if the package really exists),
    /// just that it is syntactically valid.
    fn parse_target_label(x: &str) -> anyhow::Result<TargetLabel> {
        let err = || AnonTargetsError::NotTargetLabel(x.to_owned());
        let lex = lex_target_pattern::<TargetPattern>(x, false).with_context(err)?;
        let cell = CellName::unchecked_new(lex.cell_alias.unwrap_or_default().to_owned());
        match lex.pattern.reject_ambiguity()? {
            PatternData::TargetInPackage { package, target } => Ok(TargetLabel::new(
                Package::new(&cell, CellRelativePath::new(package)),
                target,
            )),
            _ => Err(err().into()),
        }
    }

    fn create_name(rule_name: &str) -> anyhow::Result<TargetLabel> {
        let pkg = Package::new(
            &CellName::unchecked_new("anon".to_owned()),
            CellRelativePath::empty(),
        );
        Ok(TargetLabel::new(pkg, TargetName::new(rule_name)?))
    }

    fn coerce_name(x: Value) -> anyhow::Result<TargetLabel> {
        if let Some(x) = Label::from_value(x) {
            Ok(x.label().target().unconfigured().dupe())
        } else if let Some(x) = x.unpack_str() {
            Self::parse_target_label(x)
        } else {
            Err(AnonTargetsError::InvalidNameType {
                typ: x.get_type().to_owned(),
                value: x.to_string(),
            }
            .into())
        }
    }

    fn coerce_attr(attr: &Attribute, x: Value) -> anyhow::Result<ConfiguredAttr> {
        let ctx = AnonAttrCtx::new();
        let a = attr
            .coercer
            .0
            .coerce_item(AttrIsConfigurable::No, &ctx, x)?;
        a.configure(&ctx)
    }

    async fn resolve(&self, dice: &DiceComputations) -> anyhow::Result<AnalysisResult> {
        #[async_trait]
        impl Key for AnonTargetKey {
            type Value = SharedResult<AnalysisResult>;

            async fn compute(&self, ctx: &DiceComputations) -> Self::Value {
                Ok(self.run_analysis(ctx).await?)
            }

            fn equality(_: &Self::Value, _: &Self::Value) -> bool {
                false
            }
        }

        Ok(dice.compute(self).await??)
    }

    fn run_analysis<'a>(
        &'a self,
        dice: &'a DiceComputations,
    ) -> impl Future<Output = anyhow::Result<AnalysisResult>> + Send + 'a {
        let fut = async move { self.run_analysis_impl(dice).await };
        unsafe { UnsafeSendFuture::new_encapsulates_starlark(fut) }
    }

    async fn run_analysis_impl(&self, dice: &DiceComputations) -> anyhow::Result<AnalysisResult> {
        let rule_impl = get_rule_impl(dice, self.0.rule_type()).await?;
        let env = Module::new();
        let mut eval = Evaluator::new(&env);

        // No attributes are allowed to contain macros or other stuff, so an empty resolution context works
        let resolution_ctx = RuleAnalysisAttrResolutionContext {
            module: &env,
            dep_analysis_results: HashMap::new(),
            query_results: HashMap::new(),
        };

        let mut resolved_attrs = SmallMap::with_capacity(self.0.attrs().len());
        for (name, attr) in self.0.attrs().iter() {
            resolved_attrs.insert(
                env.heap().alloc_str(name),
                attr.resolve_single(&resolution_ctx)?,
            );
        }
        let attributes = env.heap().alloc(Struct::new(resolved_attrs));

        let exec_resolution = ExecutionPlatformResolution::new(
            Some(
                find_execution_platform_by_configuration(
                    dice,
                    self.0.exec_cfg(),
                    self.0.exec_cfg(),
                )
                .await?,
            ),
            Vec::new(),
        );

        let registry = AnalysisRegistry::new_from_owner(
            BaseDeferredKey::AnonTarget(self.0.dupe()),
            exec_resolution,
        );
        let ctx = env.heap().alloc_typed(AnalysisContext::new(
            eval.heap(),
            attributes,
            Some(
                eval.heap()
                    .alloc_typed(Label::new(ConfiguredProvidersLabel::new(
                        self.0.configured_label(),
                        ProvidersName::Default,
                    ))),
            ),
            registry,
        ));

        let list_res = rule_impl.invoke(&mut eval, ctx)?;
        ctx.run_promises(dice, &mut eval).await?;
        let res_typed = ProviderCollection::try_from_value(list_res)?;
        let res = env.heap().alloc(res_typed);
        env.set("", res);

        // Pull the ctx object back out, and steal ctx.action's state back
        let analysis_registry = ctx.take_state();
        let (frozen_env, deferreds) = analysis_registry.finalize(&env)(env)?;

        let res = frozen_env.get("").unwrap();
        let provider_collection = FrozenProviderCollectionValue::try_from_value(res)
            .expect("just created this, this shouldn't happen");

        // this could look nicer if we had the entire analysis be a deferred
        let deferred = DeferredTable::new(deferreds.take_result()?);
        Ok(AnalysisResult::new(provider_collection, deferred, None))
    }
}

/// Several attribute functions need a context, make one that is mostly useless.
struct AnonAttrCtx {
    cfg: Configuration,
    transitions: OrderedMap<Arc<TransitionId>, Arc<TransitionApplied>>,
}

impl AnonAttrCtx {
    fn new() -> Self {
        Self {
            cfg: Configuration::unspecified(),
            transitions: OrderedMap::new(),
        }
    }
}

impl AttrCoercionContext for AnonAttrCtx {
    fn coerce_label(&self, value: &str) -> anyhow::Result<ProvidersLabel> {
        Err(AnonTargetsError::CantParseDuringCoerce(value.to_owned()).into())
    }

    fn coerce_path(&self, value: &str, _allow_directory: bool) -> anyhow::Result<CoercedPath> {
        Err(AnonTargetsError::CantParseDuringCoerce(value.to_owned()).into())
    }

    fn coerce_target_pattern(&self, pattern: &str) -> anyhow::Result<ParsedPattern<TargetPattern>> {
        Err(AnonTargetsError::CantParseDuringCoerce(pattern.to_owned()).into())
    }

    fn visit_query_function_literals(
        &self,
        _visitor: &mut dyn buck2_query::query::syntax::simple::functions::QueryLiteralVisitor,
        _expr: &buck2_query_parser::spanned::Spanned<buck2_query_parser::Expr>,
        query: &str,
    ) -> anyhow::Result<()> {
        Err(AnonTargetsError::CantParseDuringCoerce(query.to_owned()).into())
    }
}

impl AttrConfigurationContext for AnonAttrCtx {
    fn matches<'a>(&'a self, _label: &TargetLabel) -> Option<&'a ConfigurationData> {
        None
    }

    fn cfg(&self) -> &Configuration {
        &self.cfg
    }

    fn exec_cfg(&self) -> &Configuration {
        &self.cfg
    }

    fn platform_cfg(&self, _label: &TargetLabel) -> anyhow::Result<&Configuration> {
        Ok(&self.cfg)
    }

    fn resolved_transitions(&self) -> &OrderedMap<Arc<TransitionId>, Arc<TransitionApplied>> {
        &self.transitions
    }
}

pub(crate) async fn eval_anon_target(
    dice: &DiceComputations,
    target: &Arc<AnonTarget>,
) -> anyhow::Result<AnalysisResult> {
    AnonTargetKey::ref_cast(target).resolve(dice).await
}

impl<'v> AnonTargetsRegistry<'v> {
    pub(crate) fn new(execution_platform: ExecutionPlatformResolution) -> Self {
        Self {
            execution_platform,
            entries: Vec::new(),
        }
    }

    pub(crate) fn register(
        &mut self,
        promise: ValueTyped<'v, StarlarkPromise<'v>>,
        rule: ValueTyped<'v, FrozenRuleCallable>,
        attributes: DictOf<'v, &'v str, Value<'v>>,
    ) -> anyhow::Result<()> {
        self.entries.push((
            promise,
            AnonTargetKey::new(&self.execution_platform, rule, attributes)?,
        ));
        Ok(())
    }

    pub(crate) fn get_promises(&mut self) -> Option<AnonTargetsRegistry<'v>> {
        if self.entries.is_empty() {
            None
        } else {
            // We swap it out, so we can still collect new promises
            let mut new = AnonTargetsRegistry::new(self.execution_platform.dupe());
            mem::swap(&mut new, self);
            Some(new)
        }
    }

    pub(crate) async fn run_promises(
        self,
        dice: &DiceComputations,
        eval: &mut Evaluator<'v, '_>,
    ) -> anyhow::Result<()> {
        // Resolve all the targets in parallel
        let values =
            future::try_join_all(self.entries.iter().map(|(_, target)| target.resolve(dice)))
                .await?;
        // But must bind the promises sequentially
        for ((promise, _), val) in self.entries.iter().zip(values) {
            let val = val
                .provider_collection
                .value()
                .owned_value(eval.frozen_heap());
            promise.resolve(val, eval)?
        }
        Ok(())
    }

    pub(crate) fn assert_no_promises(&self) -> anyhow::Result<()> {
        if self.entries.is_empty() {
            Ok(())
        } else {
            Err(AnonTargetsError::AssertNoPromisesFailed.into())
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn anon_target_name() {
        assert_eq!(
            AnonTargetKey::parse_target_label("//foo:bar")
                .unwrap()
                .to_string(),
            "//foo:bar"
        );
        assert_eq!(
            AnonTargetKey::parse_target_label("cell//foo/bar:baz")
                .unwrap()
                .to_string(),
            "cell//foo/bar:baz"
        );
        assert!(AnonTargetKey::parse_target_label("foo").is_err());
        assert!(AnonTargetKey::parse_target_label("//foo:").is_err());
    }
}
