//! Stage 3.5: graph validation and the legacy-emission comparison.
//!
//! `validate_specialization_graph` is the final audit of the Stage 3.3
//! worklist and its Stage 3.4 bodies. It rebuilds each instance's canonical
//! key from its pruned environment and resolved evidence, proves the graph is
//! closed and name-stable, and walks every emitted body for template-only
//! values. It runs inside `Lowerer::lower` after the Stage 3.4 body validator
//! and never interns keys, materializes bodies, or touches the legacy backend.
//!
//! The test-only `instance_for_legacy_specialization` matcher backs the
//! transition comparison in this module: the legacy LLVM specialization queue
//! records a function plus concrete substitutions only, so evidence is not
//! part of the match.

use std::collections::{BTreeSet, HashSet};

use staple_syntax::Diagnostic;

use crate::specialization::{
    CanonicalEffectSet, CanonicalType, InstanceKey, InstanceSubstitution, canonical_evidence,
};
use crate::{
    CallSubstitutions, FunctionInstanceId, Origin, TraitEvidence, contains_type_parameter,
};

use super::instance_body::LoweredInstanceBody;
use super::{ArenaId, LoweredFunctionInstance, LoweredProgram};

impl LoweredProgram {
    /// The Stage 3.5 graph audit: catalog/name agreement, complete concrete
    /// environments, resolved evidence, dependency and recursive back-edge
    /// integrity, and the absence of template-only values in emitted bodies.
    pub(super) fn validate_specialization_graph(&self) -> Vec<Diagnostic> {
        let mut validator = GraphValidator {
            program: self,
            diagnostics: Vec::new(),
        };
        validator.run();
        validator.diagnostics
    }
}

struct GraphValidator<'a> {
    program: &'a LoweredProgram,
    diagnostics: Vec<Diagnostic>,
}

impl<'a> GraphValidator<'a> {
    fn run(&mut self) {
        self.check_names();
        for (id, instance) in self.program.instances.iter() {
            self.check_instance_environment(id, instance);
            self.check_dependency_edges(id, instance);
            self.check_body_substitutions(id);
        }
    }

    fn report(&mut self, origin: &Origin, message: impl Into<String>) {
        self.diagnostics
            .push(Diagnostic::new(origin.span.clone(), message.into()));
    }

    /// Planned names must be non-empty and unique across both key families,
    /// and every interned record must carry its planned name.
    fn check_names(&mut self) {
        let names = match self.program.specializations.planned_names() {
            Ok(names) => names,
            Err(collision) => {
                self.diagnostics.push(Diagnostic::new(
                    staple_syntax::Span::Compiler,
                    format!("specialization name collision: `{}`", collision.name),
                ));
                return;
            }
        };
        let mut seen = HashSet::new();
        for name in &names {
            if name.is_empty() {
                self.diagnostics.push(Diagnostic::new(
                    staple_syntax::Span::Compiler,
                    "specialization plan contains an empty name",
                ));
                continue;
            }
            if !seen.insert(name.clone()) {
                self.diagnostics.push(Diagnostic::new(
                    staple_syntax::Span::Compiler,
                    format!("specialization name `{name}` is not unique"),
                ));
            }
        }
        let mut stored = HashSet::new();
        for (id, instance) in self.program.instances.iter() {
            if instance.name.is_empty() {
                self.report(
                    &instance.origin,
                    format!("function instance {} has no planned name", id.index()),
                );
            } else if !stored.insert(instance.name.clone()) {
                self.report(
                    &instance.origin,
                    format!("specialization name `{}` is not unique", instance.name),
                );
            }
        }
        for (id, artifact) in self.program.artifacts.iter() {
            if artifact.name.is_empty() {
                self.report(
                    &artifact.origin,
                    format!("generated artifact {} has no planned name", id.index()),
                );
            } else if !stored.insert(artifact.name.clone()) {
                self.report(
                    &artifact.origin,
                    format!("specialization name `{}` is not unique", artifact.name),
                );
            }
        }
    }

    /// Rebuild the instance key from its pruned environment and resolved
    /// evidence with the Stage 3.1 canonical converters. A leftover declared
    /// parameter, effect variable, or checker placeholder, an environment that
    /// does not hold exactly the relevant parameters, and an unresolved
    /// evidence recipe are all reported at the instance origin.
    fn check_instance_environment(
        &mut self,
        id: FunctionInstanceId,
        instance: &LoweredFunctionInstance,
    ) {
        let mut entries = BTreeSet::new();
        for (parameter, _) in instance.environment.iter() {
            entries.insert(parameter);
        }
        let relevant = instance
            .relevant
            .type_parameters()
            .chain(instance.relevant.effect_parameters())
            .collect::<BTreeSet<_>>();
        if entries != relevant {
            self.report(
                &instance.origin,
                format!(
                    "function instance {} environment does not cover exactly its relevant parameters",
                    id.index()
                ),
            );
        }
        let Some(key) = self.program.specializations.instance(instance.ordinal) else {
            return;
        };
        let mut substitutions = Vec::new();
        let mut complete = true;
        for parameter in instance.relevant.type_parameters() {
            let Some(value_type) = instance.environment.type_value(parameter) else {
                self.report(
                    &instance.origin,
                    format!(
                        "function instance {} has no concrete value for relevant type parameter {}",
                        id.index(),
                        parameter.0
                    ),
                );
                complete = false;
                continue;
            };
            if contains_type_parameter(value_type) {
                self.report(
                    &instance.origin,
                    format!(
                        "function instance {} keeps a declared type parameter in its environment: {value_type}",
                        id.index()
                    ),
                );
                complete = false;
                continue;
            }
            match CanonicalType::concrete(value_type, &instance.origin) {
                Ok(value) => substitutions.push(InstanceSubstitution::Type { parameter, value }),
                Err(diagnostic) => {
                    self.diagnostics.push(diagnostic);
                    complete = false;
                }
            }
        }
        for parameter in instance.relevant.effect_parameters() {
            let Some(effects) = instance.environment.effect_value(parameter) else {
                self.report(
                    &instance.origin,
                    format!(
                        "function instance {} has no concrete row for relevant effect parameter {}",
                        id.index(),
                        parameter.0
                    ),
                );
                complete = false;
                continue;
            };
            if effects.variable.is_some() {
                self.report(
                    &instance.origin,
                    format!(
                        "function instance {} keeps a declared effect variable in its environment: {effects}",
                        id.index()
                    ),
                );
                complete = false;
                continue;
            }
            match CanonicalEffectSet::concrete(effects, &instance.origin) {
                Ok(effects) => {
                    substitutions.push(InstanceSubstitution::Effect { parameter, effects })
                }
                Err(diagnostic) => {
                    self.diagnostics.push(diagnostic);
                    complete = false;
                }
            }
        }
        let evidence = match &instance.evidence {
            Some(evidence) => match canonical_evidence(evidence, &instance.origin) {
                Ok(evidence) => Some(evidence),
                Err(diagnostic) => {
                    self.diagnostics.push(diagnostic);
                    complete = false;
                    None
                }
            },
            None => None,
        };
        if !complete {
            return;
        }
        match InstanceKey::new(instance.template, substitutions, evidence) {
            Ok(rebuilt) if &rebuilt == key => {}
            Ok(_) => self.report(
                &instance.origin,
                format!(
                    "function instance {} environment and evidence do not rebuild its catalog key",
                    id.index()
                ),
            ),
            Err(error) => self.report(&instance.origin, error.message()),
        }
    }

    /// Dependency edges stay inside the graph and keep key identity: a
    /// recursive back-edge targets the owner instance, and an edge to another
    /// instance never carries the owner's key. The catalog interns one ordinal
    /// per key, so both conditions also prove same-key recursion converged.
    fn check_dependency_edges(
        &mut self,
        id: FunctionInstanceId,
        instance: &LoweredFunctionInstance,
    ) {
        let owner_key = self.program.specializations.instance(instance.ordinal);
        for dependency in &instance.dependencies {
            let Some(target) = self.program.instances.get(dependency.instance) else {
                self.report(
                    &dependency.origin,
                    format!(
                        "function instance {} depends on missing instance {}",
                        id.index(),
                        dependency.instance.index()
                    ),
                );
                continue;
            };
            let (Some(owner), Some(target)) = (
                owner_key,
                self.program.specializations.instance(target.ordinal),
            ) else {
                continue;
            };
            let same_key = owner == target;
            let same_instance = dependency.instance == id;
            if same_key && !same_instance {
                self.report(
                    &dependency.origin,
                    format!(
                        "function instance {} depends on a different instance with the same specialization key",
                        id.index()
                    ),
                );
            }
            if same_instance && !same_key {
                self.report(
                    &dependency.origin,
                    format!(
                        "function instance {} has a recursive back-edge whose key is not its own",
                        id.index()
                    ),
                );
            }
        }
    }

    /// Walk one dependency target's emitted body for template-only checked
    /// values. Arena nodes are visited once per instance so shared memoized
    /// sites are audited once.
    fn check_body_substitutions(&mut self, instance: FunctionInstanceId) {
        let Some(record) = self.program.instances.get(instance) else {
            return;
        };
        let Some(body) = &record.body else {
            return;
        };
        if body.template != record.template {
            self.report(
                &body.origin,
                format!(
                    "function instance {} body template disagrees with its instance",
                    instance.index()
                ),
            );
            return;
        }
        for (_, call) in body.calls.iter() {
            self.check_substitutions(&call.origin, &call.substitutions, "call");
            if let Some(evidence) = &call.evidence {
                self.check_evidence(&call.origin, evidence);
            }
        }
        for (_, value) in body.callable_values.iter() {
            self.check_substitutions(&value.origin, &value.substitutions, "callable value");
            if let Some(evidence) = &value.evidence {
                self.check_evidence(&value.origin, evidence);
            }
            let Some(closure) = &value.closure else {
                continue;
            };
            self.check_substitutions(
                &value.origin,
                &closure.substitutions,
                "closure construction",
            );
            for capture in &closure.captures {
                if contains_type_parameter(&capture.value_type) {
                    self.report(
                        &value.origin,
                        format!(
                            "emitted callable value keeps a template capture type: {}",
                            capture.value_type
                        ),
                    );
                }
            }
            if let Some(template) = self.program.functions.get(closure.function) {
                let expected = template
                    .captures
                    .iter()
                    .map(|capture| capture.symbol)
                    .collect::<Vec<_>>();
                let actual = closure
                    .captures
                    .iter()
                    .map(|capture| capture.capture.symbol)
                    .collect::<Vec<_>>();
                if expected != actual {
                    self.report(
                        &value.origin,
                        "emitted closure capture order disagrees with its template",
                    );
                }
            }
        }
        for (site, evidence) in &body.evidence {
            let origin = body_evidence_origin(body, *site).unwrap_or_else(Origin::compiler);
            self.check_evidence(&origin, evidence);
        }
    }

    fn check_substitutions(
        &mut self,
        origin: &Origin,
        substitutions: &CallSubstitutions,
        what: &str,
    ) {
        for entry in &substitutions.types {
            if contains_type_parameter(&entry.value_type) {
                self.report(
                    origin,
                    format!(
                        "emitted {what} keeps a template type substitution for parameter {}: {}",
                        entry.parameter.0, entry.value_type
                    ),
                );
            }
        }
        for entry in &substitutions.effects {
            if entry.effects.variable.is_some() {
                self.report(
                    origin,
                    format!(
                        "emitted {what} keeps a template effect substitution for parameter {}: {}",
                        entry.parameter.0, entry.effects
                    ),
                );
            }
        }
    }

    fn check_evidence(&mut self, origin: &Origin, evidence: &TraitEvidence) {
        if matches!(
            evidence,
            TraitEvidence::DeclaredBound { .. } | TraitEvidence::RejectedImplementation { .. }
        ) {
            self.report(
                origin,
                "emitted instance retains an unresolved trait evidence recipe",
            );
        }
    }
}

/// The origin of one body evidence-table entry, used for diagnostics only.
fn body_evidence_origin(
    body: &LoweredInstanceBody,
    site: super::LoweredBindingSite,
) -> Option<Origin> {
    use super::LoweredBindingSite;
    match site {
        LoweredBindingSite::Call(id) => body.calls.get(id).map(|call| call.origin.clone()),
        LoweredBindingSite::CallableValue(id) => body
            .callable_values
            .get(id)
            .map(|value| value.origin.clone()),
        LoweredBindingSite::CallArgumentThunk { call, .. } => {
            body.calls.get(call).map(|call| call.origin.clone())
        }
        LoweredBindingSite::Index(id)
        | LoweredBindingSite::FormattingConstructor(id)
        | LoweredBindingSite::FormattingFinish(id)
        | LoweredBindingSite::Interpolation { template: id, .. } => body
            .expressions
            .get(id)
            .map(|expression| expression.origin.clone()),
        LoweredBindingSite::IndexedAssignment(id) => {
            body.items.get(id).map(|item| item.origin.clone())
        }
        LoweredBindingSite::DerivedEvaluator(id) => body
            .reactive_operations
            .get(id)
            .map(|operation| operation.origin.clone()),
        LoweredBindingSite::ReactiveCallback(id) => body
            .reactive_callbacks
            .get(id)
            .map(|callback| callback.origin.clone()),
        LoweredBindingSite::Coro(id) => body.coros.get(id).map(|coro| coro.origin.clone()),
        LoweredBindingSite::AwaitChildPlan(id) => {
            body.awaits.get(id).map(|await_| await_.origin.clone())
        }
    }
}

#[cfg(test)]
impl LoweredProgram {
    /// Test-only: the interned instance whose template and concrete
    /// substitutions reproduce one legacy specialization. The legacy queue
    /// records only the concrete callable type, so evidence is excluded from
    /// the comparison.
    pub(crate) fn instance_for_legacy_specialization(
        &self,
        function: crate::FunctionId,
        substitutions: &std::collections::HashMap<crate::TypeParameterId, crate::CheckedType>,
    ) -> Option<FunctionInstanceId> {
        self.instances.iter().find_map(|(id, instance)| {
            if instance.template != function {
                return None;
            }
            let types_match = instance.relevant.type_parameters().all(|parameter| {
                matches!(
                    (
                        instance.environment.type_value(parameter),
                        substitutions.get(&parameter),
                    ),
                    (Some(instance_value), Some(legacy)) if instance_value == legacy
                )
            });
            let effects_match = instance.relevant.effect_parameters().all(|parameter| {
                match (
                    instance.environment.effect_value(parameter),
                    substitutions
                        .get(&parameter)
                        .and_then(crate::effect_substitution_value),
                ) {
                    (Some(instance_value), Some(legacy)) => instance_value == legacy,
                    _ => false,
                }
            });
            (types_match && effects_match).then_some(id)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use inkwell::context::Context;

    use crate::specialization::{ArtifactRequestKey, CanonicalFunctionType};
    use crate::{
        CallTypeSubstitution, LoweredModule, Lowerer, NameResolver, ProgramLoader,
        SubstitutionEnvironment, TypeChecker, TypeParameterId, TypedModule,
    };

    use super::super::{
        LoweredBoundTarget, LoweredCallStep, LoweredCallableCategory, LoweredCallableTarget,
        LoweredExpressionKind, LoweredInstanceDependency, LoweredInstanceDependencyKind,
        LoweredInstanceRequest, LoweredRepeatCount, TraitEvidence,
    };
    use super::*;

    fn standard_library_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("compiler crate should have a workspace parent")
            .join("stdlib")
    }

    fn checked_program(source: &str) -> TypedModule {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("compiler crate should have a workspace parent");
        let program = ProgramLoader::new()
            .with_standard_library_root(standard_library_root())
            .load_source(source, root)
            .expect("test source should load");
        let resolved = NameResolver::new()
            .resolve_program(program)
            .expect("test source should resolve");
        TypeChecker::new()
            .check(resolved)
            .expect("test source should type check")
    }

    fn lower(source: &str) -> LoweredModule {
        let module = checked_program(source);
        Lowerer::new()
            .lower(&module)
            .expect("checked source should lower, materialize, and validate")
    }

    /// Builds the graph without installing bodies so a test can corrupt it.
    fn lower_graph(source: &str) -> LoweredProgram {
        let module = checked_program(source);
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());
        let diagnostics = program.build_specialization_worklist();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate_specializations().is_empty());
        assert!(program.validate_specialization_graph().is_empty());
        program
    }

    fn function_id(program: &LoweredProgram, name: &str) -> crate::FunctionId {
        program
            .functions
            .iter()
            .find(|(_, _, function)| {
                function.name == name || function.name.ends_with(&format!(".{name}"))
            })
            .map(|(_, id, _)| id)
            .unwrap_or_else(|| panic!("no lowered function named {name}"))
    }

    fn instances_of<'a>(
        program: &'a LoweredProgram,
        template: crate::FunctionId,
    ) -> Vec<(FunctionInstanceId, &'a LoweredFunctionInstance)> {
        program
            .instances
            .iter()
            .filter(|(_, instance)| instance.template == template)
            .collect()
    }

    fn single_instance<'a>(
        program: &'a LoweredProgram,
        name: &str,
    ) -> (FunctionInstanceId, &'a LoweredFunctionInstance) {
        let template = function_id(program, name);
        let mut instances = instances_of(program, template);
        assert_eq!(
            instances.len(),
            1,
            "expected exactly one instance of {name}: {instances:?}"
        );
        instances.pop().expect("instance")
    }

    fn dependency_of_kind<'a>(
        program: &'a LoweredProgram,
        instance: &'a LoweredFunctionInstance,
        kind: LoweredInstanceDependencyKind,
    ) -> (FunctionInstanceId, &'a LoweredFunctionInstance) {
        for dependency in &instance.dependencies {
            if dependency.kind == kind {
                let (id, target) = program
                    .instances
                    .get(dependency.instance)
                    .map(|target| (dependency.instance, target))
                    .expect("dependency target is interned");
                return (id, target);
            }
        }
        panic!("no {:?} dependency in {}", kind, instance.name);
    }

    // ------------------------------------------------------------------
    // Graph validator corruption tests.
    // ------------------------------------------------------------------

    const TWO_SUBSTITUTIONS: &str = concat!(
        "def identity: <T where Copy T> T -> T = value => value\n",
        "let first: I32 = identity 1\n",
        "let second: U8 = identity (1 satisfies U8)\n",
    );

    #[test]
    fn graph_validator_reports_an_environment_that_does_not_cover_relevance() {
        let mut program = lower_graph(TWO_SUBSTITUTIONS);
        let id = program
            .instances
            .iter()
            .find(|(_, instance)| !instance.relevant.is_empty())
            .map(|(id, _)| id)
            .expect("a substituted instance");
        program.instances.get_mut(id).expect("instance").environment =
            SubstitutionEnvironment::default();
        let diagnostics = program.validate_specialization_graph();
        assert!(
            diagnostics.iter().any(|diagnostic| diagnostic
                .message
                .contains("does not cover exactly its relevant parameters")),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn graph_validator_reports_unresolved_instance_evidence() {
        let mut program = lower_graph(TWO_SUBSTITUTIONS);
        let id = program
            .instances
            .iter()
            .next()
            .map(|(id, _)| id)
            .expect("an instance");
        program.instances.get_mut(id).expect("instance").evidence =
            Some(TraitEvidence::DeclaredBound {
                trait_id: crate::TraitId(0),
                method: None,
                arguments: Vec::new(),
                prerequisites: Vec::new(),
            });
        let diagnostics = program.validate_specialization_graph();
        assert!(
            diagnostics.iter().any(|diagnostic| {
                diagnostic.message.contains("declared bound")
                    || diagnostic.message.contains("unresolved trait evidence")
            }),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn graph_validator_reports_duplicate_instance_names() {
        let mut program = lower_graph(TWO_SUBSTITUTIONS);
        let ids = program
            .instances
            .iter()
            .map(|(id, _)| id)
            .collect::<Vec<_>>();
        let first = ids[0];
        let second = ids[1];
        let name = program
            .instances
            .get(first)
            .expect("first instance")
            .name
            .clone();
        program.instances.get_mut(second).expect("instance").name = name;
        let diagnostics = program.validate_specialization_graph();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("is not unique")),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn graph_validator_reports_a_missing_dependency_target() {
        let mut program = lower_graph(TWO_SUBSTITUTIONS);
        let origin = program
            .instances
            .iter()
            .next()
            .map(|(_, instance)| instance.origin.clone())
            .expect("an instance");
        let id = program
            .instances
            .iter()
            .next()
            .map(|(id, _)| id)
            .expect("instance");
        program
            .instances
            .get_mut(id)
            .expect("instance")
            .dependencies
            .push(LoweredInstanceDependency {
                instance: FunctionInstanceId::for_test(999),
                origin,
                kind: LoweredInstanceDependencyKind::DirectCall,
            });
        let diagnostics = program.validate_specialization_graph();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("depends on missing instance")),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn graph_validator_reports_template_only_body_substitutions() {
        let module = checked_program(concat!(
            "def identity: <T where Copy T> T -> T = value => value\n",
            "let applied: I32 = identity 1\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.build_specialization_worklist().is_empty());
        assert!(program.materialize_instance_bodies().is_empty());
        let id = program
            .instances
            .iter()
            .find(|(_, instance)| instance.body.is_some())
            .map(|(id, _)| id)
            .expect("an instance body");
        let body = program
            .instances
            .get_mut(id)
            .and_then(|instance| instance.body.as_mut())
            .expect("body");
        let (call_id, _) = body
            .calls
            .iter()
            .next()
            .expect("the identity body calls its boxed value");
        body.calls
            .get_mut(call_id)
            .expect("call")
            .substitutions
            .types
            .push(CallTypeSubstitution {
                parameter: TypeParameterId(0),
                value_type: crate::CheckedType::Parameter {
                    id: TypeParameterId(0),
                    name: "T".to_owned(),
                    sized: true,
                },
            });
        let diagnostics = program.validate_specialization_graph();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("template type substitution")),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn materialization_is_a_fixed_point() {
        let module = checked_program(concat!(
            "def inner: <T where Copy T> T -> T = value => value\n",
            "def outer: <T where Copy T> T -> T = value => inner value\n",
            "let applied: I32 = outer 1\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.build_specialization_worklist().is_empty());
        let instances = program.instances.len();
        let edges = program
            .instances
            .iter()
            .map(|(_, instance)| instance.dependencies.len())
            .collect::<Vec<_>>();
        assert!(program.materialize_instance_bodies().is_empty());
        assert_eq!(
            program.instances.len(),
            instances,
            "materialization must not intern new instances"
        );
        assert_eq!(
            program
                .instances
                .iter()
                .map(|(_, instance)| instance.dependencies.len())
                .collect::<Vec<_>>(),
            edges,
            "materialization must not discover new dependency edges"
        );
        assert!(program.validate_instance_bodies().is_empty());
        assert!(program.validate_specialization_graph().is_empty());
    }

    // ------------------------------------------------------------------
    // Stage 3.5 fixtures over the full lowering pipeline.
    // ------------------------------------------------------------------

    #[test]
    fn direct_and_indirect_calls_keep_distinct_routes() {
        let lowered = lower(concat!(
            "def identity: <T where Copy T> T -> T = value => value\n",
            "def apply: (I32 -> I32) -> I32 -> I32 = f => value => f value\n",
            "let applied: I32 = apply identity 1\n",
        ));
        let program = &lowered.program;
        let (apply_id, apply_instance) = single_instance(program, "apply");
        // The inner lambda is its own nested function instance; the indirect
        // invocation lives there.
        let (_, lambda) = dependency_of_kind(
            program,
            apply_instance,
            LoweredInstanceDependencyKind::CallableValue,
        );
        let body = lambda.body.as_ref().expect("inner lambda body");
        let indirect = body
            .calls
            .iter()
            .find(|(_, call)| matches!(call.target, LoweredCallableTarget::IndirectClosure { .. }))
            .map(|(id, _)| id)
            .expect("apply invokes its closure indirectly");
        assert!(matches!(
            body.binding(super::super::LoweredBindingSite::Call(indirect)),
            Some(LoweredBoundTarget::Route(
                LoweredCallableCategory::IndirectClosure
            ))
        ));
        let (identity_id, identity_instance) = single_instance(program, "identity");
        assert_eq!(
            identity_instance.environment.type_value(
                identity_instance
                    .relevant
                    .type_parameters()
                    .next()
                    .expect("T")
            ),
            Some(&crate::CheckedType::I32)
        );
        assert!(
            matches!(
                identity_instance.request,
                LoweredInstanceRequest::Initializer {
                    kind: LoweredInstanceDependencyKind::CallableValue,
                    ..
                }
            ),
            "the function value is a callable-value dependency: {:?}",
            identity_instance.request
        );
        assert_ne!(apply_id, identity_id);
    }

    #[test]
    fn result_only_generics_infer_from_the_complete_callable_type() {
        let lowered = lower(concat!(
            "type Phantom T = ctor ()\n",
            "def phantom_result: <T> () -> Phantom T = () => Phantom ()\n",
            "let hidden: Phantom I32 = phantom_result ()\n",
        ));
        let program = &lowered.program;
        let (_, instance) = single_instance(program, "phantom_result");
        let parameter = instance
            .relevant
            .type_parameters()
            .next()
            .expect("T is relevant");
        assert_eq!(
            instance.environment.type_value(parameter),
            Some(&crate::CheckedType::I32)
        );
        assert!(
            !contains_type_parameter(&crate::CheckedType::Function(
                instance.body.as_ref().expect("body").signature.clone()
            )),
            "the result-only parameter is concrete in the body"
        );
    }

    #[test]
    fn curried_layers_and_repeated_products_produce_one_concrete_body() {
        let lowered = lower(concat!(
            "def repeat: <T, N where Copy T, Natural N> T -> N -> (T; N) = value => n => (value; N)\n",
            "let repeated: (I32; 3) = repeat 7 3\n",
        ));
        let program = &lowered.program;
        let (_, instance) = single_instance(program, "repeat");
        let mut values = instance
            .relevant
            .type_parameters()
            .map(|parameter| {
                instance
                    .environment
                    .type_value(parameter)
                    .expect("relevant parameter is concrete")
                    .clone()
            })
            .collect::<Vec<_>>();
        values.sort_by_key(|value| format!("{value:?}"));
        assert!(values.contains(&crate::CheckedType::I32));
        assert!(
            values.iter().any(|value| matches!(
                value,
                crate::CheckedType::NumberLiteral(3) | crate::CheckedType::USize
            )),
            "the repeated count resolves: {values:?}"
        );
        // The repeated product survives into exactly one concrete body, in
        // the outer or the nested lambda instance.
        let mut repeated_products = 0;
        for (_, candidate) in program.instances.iter() {
            let Some(body) = &candidate.body else {
                continue;
            };
            for (_, expression) in body.expressions.iter() {
                if let LoweredExpressionKind::RepeatedProduct(product) = &expression.kind {
                    repeated_products += 1;
                    match &product.count {
                        LoweredRepeatCount::Fixed(count) => assert_eq!(*count, 3),
                        LoweredRepeatCount::Symbolic(count) => {
                            assert!(!contains_type_parameter(count));
                        }
                    }
                }
            }
        }
        assert_eq!(repeated_products, 1, "one concrete repeated product");
    }

    #[test]
    fn same_signature_generic_captures_stay_distinct() {
        let lowered = lower(concat!(
            "def show_thunk: <T where Copy T, Display T> move T -> (() -> String) = move value => () => \"value=$value\"\n",
            "let shown_number = show_thunk 1\n",
            "let shown_byte = show_thunk (1 satisfies U8)\n",
        ));
        let program = &lowered.program;
        let template = function_id(program, "show_thunk");
        let outer = instances_of(program, template);
        assert_eq!(outer.len(), 2, "one outer instance per substitution");
        let mut closure_instances = Vec::new();
        for (_, instance) in &outer {
            let (_, closure) = dependency_of_kind(
                program,
                instance,
                LoweredInstanceDependencyKind::CallableValue,
            );
            assert_eq!(
                closure
                    .body
                    .as_ref()
                    .expect("closure body")
                    .signature
                    .result
                    .as_ref(),
                &crate::CheckedType::String,
                "both closures return String"
            );
            closure_instances.push(closure);
        }
        let mut captures = closure_instances
            .iter()
            .map(|instance| {
                instance
                    .body
                    .as_ref()
                    .expect("closure body")
                    .captures
                    .iter()
                    .map(|capture| capture.value_type.clone())
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        captures.sort_by_key(|capture_types| format!("{capture_types:?}"));
        assert_eq!(
            captures,
            vec![vec![crate::CheckedType::I32], vec![crate::CheckedType::U8]],
            "identical callable signatures keep distinct capture substitutions"
        );
    }

    #[test]
    fn constructors_and_structural_selections_reserve_typed_artifacts() {
        let lowered = lower(concat!(
            "type Point = ctor (I32, I32)\n",
            "let make: () -> ((I32, I32) -> Point) = () => Point\n",
            "let p = (1, 2)\n",
            "let text = \"${p:?}\"\n",
        ));
        let program = &lowered.program;
        let mut saw_adapter = false;
        let mut saw_structural = false;
        for (_, artifact) in program.artifacts.iter() {
            match program.specializations.artifact(artifact.ordinal) {
                Some(ArtifactRequestKey::ConstructorAdapter(_)) => saw_adapter = true,
                Some(ArtifactRequestKey::StructuralMethod(key)) => {
                    saw_structural = true;
                    assert!(
                        key.arguments
                            .iter()
                            .all(|argument| argument.unresolved_parameter().is_none()),
                        "structural artifact arguments are concrete"
                    );
                }
                None => panic!("artifact without a catalog key"),
            }
        }
        assert!(
            saw_adapter,
            "a constructor value reserves an adapter artifact"
        );
        assert!(
            saw_structural,
            "a structural debug interpolation reserves a structural artifact"
        );
        // Constructor and structural sites bind the artifact ordinal in bodies.
        let mut bound_artifacts = 0;
        for (_, instance) in program.instances.iter() {
            if let Some(body) = &instance.body {
                bound_artifacts += body
                    .bindings
                    .values()
                    .filter(|target| matches!(target, LoweredBoundTarget::Artifact(_)))
                    .count();
            }
        }
        assert!(
            bound_artifacts >= 1,
            "constructor and structural sites bind their artifacts: {bound_artifacts}"
        );
    }

    #[test]
    fn conditional_implementations_and_functional_dependencies_select_methods() {
        let source = concat!(
            "trait TestShow T { test_show: T -> Bool }\n",
            "impl TestShow I32 { def test_show = _ => True }\n",
            "trait TestGuarded T { guarded: T -> Bool }\n",
            "impl<T where TestShow T> TestGuarded T { def guarded = value => test_show value }\n",
            "def use_guarded: <T where TestGuarded T> T -> Bool = value => guarded value\n",
            "let ok: Bool = use_guarded 1\n",
        );
        let module = checked_program(source);
        let lowered = Lowerer::new()
            .lower(&module)
            .expect("conditional implementation source lowers");
        let program = &lowered.program;
        let (_, instance) = single_instance(program, "use_guarded");
        let (_, method) = dependency_of_kind(
            program,
            instance,
            LoweredInstanceDependencyKind::TraitMethod,
        );
        let trait_id = program
            .traits
            .iter()
            .find(|(_, _, metadata)| metadata.name == "TestGuarded")
            .map(|(_, id, _)| id)
            .expect("TestGuarded trait");
        let method_id = program.traits.get(trait_id).expect("trait").methods[0];
        let expected = module
            .trait_impl_method(trait_id, &[crate::CheckedType::I32], method_id)
            .expect("the checker selects the conditional implementation");
        assert_eq!(method.template, expected);

        let lowered = lower(concat!(
            "trait TestConvert Target Position Output where {Target, Position} ~> Output {\n",
            "  test_convert: (Target, Position) -> Output\n",
            "}\n",
            "impl TestConvert I32 I32 I32 { def test_convert = pair => pair.0 }\n",
            "def call_convert: (I32, I32) -> I32 = pair => test_convert pair\n",
            "let converted: I32 = call_convert (1, 2)\n",
        ));
        let program = &lowered.program;
        let (_, instance) = single_instance(program, "call_convert");
        let (_, method) = dependency_of_kind(
            program,
            instance,
            LoweredInstanceDependencyKind::TraitMethod,
        );
        assert!(
            !method
                .body
                .as_ref()
                .expect("body")
                .signature
                .result
                .as_ref()
                .eq(&crate::CheckedType::Error),
            "the functional-dependency position completes to I32"
        );
    }

    #[test]
    fn effect_polymorphic_callbacks_resolve_a_concrete_row() {
        let lowered = lower(concat!(
            "use std.io.IO\n",
            "def evaluate: <T, effect E> (() ->{E} T) ->{E} T = callback => callback ()\n",
            "def with_io: () ->{IO} () = () => ()\n",
            "def call_io: () ->{IO} I32 = () => evaluate { with_io (); 0 }\n",
            "let result: I32 = call_io ()\n",
        ));
        let program = &lowered.program;
        let (_, instance) = single_instance(program, "evaluate");
        let effect_parameter = instance
            .relevant
            .effect_parameters()
            .next()
            .expect("E is relevant");
        let effects = instance
            .environment
            .effect_value(effect_parameter)
            .expect("E resolves to a concrete row");
        assert!(effects.variable.is_none(), "{effects:?}");
        assert!(
            !effects.resources.is_empty(),
            "the IO resource stays in the concrete row: {effects:?}"
        );
        let body = instance.body.as_ref().expect("body");
        assert!(
            body.signature.effects.variable.is_none(),
            "the body signature carries the concrete row"
        );
    }

    #[test]
    fn defaults_evaluate_through_their_call_steps() {
        // The generic call site lives inside `call_fill` so the default step
        // is part of an emitted instance body.
        let lowered = lower(concat!(
            "def fill: <T where Copy T> (T, x: I32 = 7) -> T = (value, x) => value\n",
            "def call_fill: <T where Copy T> T -> T = value => fill (value)\n",
            "let filled: I32 = call_fill 1\n",
        ));
        let program = &lowered.program;
        let (_, fill) = single_instance(program, "fill");
        assert_eq!(
            fill.environment
                .type_value(fill.relevant.type_parameters().next().expect("T")),
            Some(&crate::CheckedType::I32)
        );
        let (_, call_fill) = single_instance(program, "call_fill");
        let body = call_fill.body.as_ref().expect("body");
        let mut defaults = 0;
        for (_, call) in body.calls.iter() {
            for step in &call.steps {
                if let LoweredCallStep::Default { slot, .. } = step {
                    assert_eq!(*slot, 1, "the default fills the omitted slot");
                    defaults += 1;
                }
            }
        }
        assert_eq!(defaults, 1, "the omitted default evaluates once");
    }

    #[test]
    fn cross_module_calls_discover_their_dependencies() {
        let root = std::env::temp_dir().join(format!(
            "staple-stage-3-5-cross-module-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("temp root");
        std::fs::write(
            root.join("tools.sta"),
            "pub mod\npub def identity: <T where Copy T> T -> T = value => value\n",
        )
        .expect("write module");
        let entry = root.join("main.sta");
        let module = {
            let program = ProgramLoader::new()
                .with_standard_library_root(standard_library_root())
                .with_module_root(&root)
                .load_source_at(&entry, "use tools.identity\nlet copy: I32 = identity 1\n")
                .expect("source should load");
            let resolved = NameResolver::new()
                .resolve_program(program)
                .expect("source should resolve");
            TypeChecker::new()
                .check(resolved)
                .expect("source should type check")
        };
        let lowered = Lowerer::new()
            .lower(&module)
            .expect("cross-module source lowers");
        let program = &lowered.program;
        let (_, instance) = single_instance(program, "identity");
        assert_eq!(
            instance
                .environment
                .type_value(instance.relevant.type_parameters().next().expect("T")),
            Some(&crate::CheckedType::I32)
        );
        assert!(
            matches!(
                instance.request,
                LoweredInstanceRequest::Initializer {
                    kind: LoweredInstanceDependencyKind::DirectCall,
                    ..
                }
            ),
            "the imported call is discovered from the entry initializer: {:?}",
            instance.request
        );
        std::fs::remove_dir_all(root).expect("clean temp root");
    }

    #[test]
    fn coroutine_and_reactive_thunks_bind_demand_driven() {
        let lowered = lower(concat!(
            "use std.coroutine.*\n",
            "def task: () -> Coroutine{} I32 = () => coro { 1 }\n",
            "def unused_task: <T where Copy T> T -> Coroutine{} T = value => coro { value }\n",
            "let created = task ()\n",
        ));
        let program = &lowered.program;
        let mut coroutine_instances = 0;
        for (_, instance) in program.instances.iter() {
            let is_thunk = program
                .functions
                .get(instance.template)
                .is_some_and(|function| function.class.coroutine_body);
            if is_thunk {
                coroutine_instances += 1;
                let body = instance.body.as_ref().expect("coroutine body");
                assert!(body.plan_template.is_some(), "the thunk owns its plan");
            }
        }
        assert_eq!(
            coroutine_instances, 1,
            "only the reachable coroutine body thunk is interned"
        );
        let template = function_id(program, "task");
        let (_, task) = instances_of(program, template)
            .into_iter()
            .next()
            .expect("task instance");
        let body = task.body.as_ref().expect("task body");
        let bound = body.bindings.values().any(|target| match target {
            LoweredBoundTarget::Instance(_) => true,
            _ => false,
        });
        assert!(bound, "the creation site binds its body thunk instance");

        let lowered = lower(concat!(
            "let signal count = 0\n",
            "let doubled = count + count\n",
            "reaction { () }\n",
        ));
        let program = &lowered.program;
        let mut evaluator_bound = false;
        let mut callback_bound = false;
        for (_, instance) in program.instances.iter() {
            let LoweredInstanceRequest::Initializer { kind, .. } = &instance.request else {
                continue;
            };
            match kind {
                LoweredInstanceDependencyKind::DerivedEvaluator => evaluator_bound = true,
                LoweredInstanceDependencyKind::ReactiveCallback
                | LoweredInstanceDependencyKind::ImplicitThunkArgument => callback_bound = true,
                _ => {}
            }
        }
        assert!(evaluator_bound, "the derived evaluator thunk is interned");
        assert!(callback_bound, "the reaction callback thunk is interned");
    }

    // ------------------------------------------------------------------
    // Legacy-emission transition comparison.
    // ------------------------------------------------------------------

    /// Returns `(source specializations, constructor adapters, structural
    /// methods)` the legacy backend emitted, all matched to the new graph.
    fn assert_legacy_emissions_are_represented(source: &str) -> (usize, usize, usize) {
        let module = checked_program(source);
        let lowered = Lowerer::new()
            .lower(&module)
            .expect("source should lower before the legacy comparison");
        let context = Context::create();
        let legacy = crate::codegen::legacy_emissions(&context, &lowered)
            .expect("the legacy backend should compile the module");
        let program = &lowered.program;
        for (function, _function_type, substitutions) in &legacy.specializations {
            let name = program
                .functions
                .get(*function)
                .map(|template| template.name.as_str())
                .unwrap_or("<missing>");
            assert!(
                program
                    .instance_for_legacy_specialization(*function, substitutions)
                    .is_some(),
                "legacy specialization for `{name}` has no matching instance"
            );
        }
        let origin = Origin::compiler();
        for (symbol, function_type) in &legacy.constructor_adapters {
            let callable_type = CanonicalFunctionType::concrete(function_type, &origin)
                .expect("a legacy adapter type is concrete");
            let found = program.artifacts.iter().any(|(_, artifact)| {
                matches!(
                    program.specializations.artifact(artifact.ordinal),
                    Some(ArtifactRequestKey::ConstructorAdapter(key))
                        if key.symbol == *symbol && key.callable_type == callable_type
                )
            });
            assert!(
                found,
                "legacy constructor adapter for symbol {} has no artifact request",
                symbol.0
            );
        }
        for (structural, arguments, _function_type) in &legacy.structural_methods {
            let arguments = arguments
                .iter()
                .map(|argument| {
                    CanonicalType::concrete(argument, &origin)
                        .expect("a legacy structural argument is concrete")
                })
                .collect::<Vec<_>>();
            let found = program.artifacts.iter().any(|(_, artifact)| {
                matches!(
                    program.specializations.artifact(artifact.ordinal),
                    Some(ArtifactRequestKey::StructuralMethod(key))
                        if key.structural == *structural && key.arguments == arguments
                )
            });
            assert!(
                found,
                "legacy structural method {structural:?} has no artifact request"
            );
        }
        (
            legacy.specializations.len(),
            legacy.constructor_adapters.len(),
            legacy.structural_methods.len(),
        )
    }

    #[test]
    fn legacy_emissions_are_represented_in_the_graph() {
        let (mut specializations, mut constructor_adapters, mut structural_methods) = (0, 0, 0);
        for source in [
            concat!(
                "def identity: <T where Copy T> T -> T = value => value\n",
                "def apply: (I32 -> I32) -> I32 -> I32 = f => value => f value\n",
                "let direct: I32 = identity 1\n",
                "let indirect: I32 = apply identity 1\n",
            ),
            concat!(
                "type Phantom T = ctor ()\n",
                "def phantom_result: <T> () -> Phantom T = () => Phantom ()\n",
                "let hidden: Phantom I32 = phantom_result ()\n",
            ),
            concat!(
                "trait TestShow T { test_show: T -> Bool }\n",
                "impl TestShow I32 { def test_show = _ => True }\n",
                "impl TestShow U8 { def test_show = _ => False }\n",
                "def show_bound: <T where TestShow T> T -> Bool = value => test_show value\n",
                "let shown: Bool = show_bound 1\n",
                "let other: Bool = show_bound (1 satisfies U8)\n",
            ),
            concat!(
                "type Point = ctor (I32, I32)\n",
                "def render: <T where Display T> move T -> String = move value => \"value=$value\"\n",
                "let text: String = render 1\n",
                "let make: () -> ((I32, I32) -> Point) = () => Point\n",
                "let point = make () (1, 2)\n",
                "let pair = (1, 2)\n",
                "let debug = \"${pair:?}\"\n",
            ),
            concat!(
                "use std.coroutine.*\n",
                "def task: () -> Coroutine{} I32 = () => coro { 1 }\n",
                "let created = task ()\n",
                "let signal count = 0\n",
                "let doubled = count + count\n",
                "reaction { () }\n",
            ),
        ] {
            let (found_specializations, found_adapters, found_structural) =
                assert_legacy_emissions_are_represented(source);
            specializations += found_specializations;
            constructor_adapters += found_adapters;
            structural_methods += found_structural;
        }
        assert!(
            specializations > 0,
            "the comparison must see at least one queued specialization"
        );
        assert!(
            constructor_adapters > 0,
            "the comparison must see at least one constructor adapter"
        );
        assert!(
            structural_methods > 0,
            "the comparison must see at least one structural method"
        );
    }

    #[test]
    fn legacy_specializations_not_in_the_catalog_are_detected() {
        // A key with a made-up substitution must not match any instance, so
        // the matcher cannot silently accept a missing specialization.
        let lowered = lower(concat!(
            "def identity: <T where Copy T> T -> T = value => value\n",
            "let applied: I32 = identity 1\n",
        ));
        let program = &lowered.program;
        let template = function_id(program, "identity");
        let parameter = program
            .instances
            .iter()
            .find(|(_, instance)| instance.template == template)
            .and_then(|(_, instance)| instance.relevant.type_parameters().next())
            .expect("T");
        let mut substitutions = std::collections::HashMap::new();
        substitutions.insert(parameter, crate::CheckedType::U64);
        assert!(
            program
                .instance_for_legacy_specialization(template, &substitutions)
                .is_none(),
            "a substitution with no instance must not match"
        );
    }
}
