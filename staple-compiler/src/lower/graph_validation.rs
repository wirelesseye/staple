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
use super::{ArenaId, LoweredFunctionInstance, LoweredItemKind, LoweredProgram};

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
        for (_, item) in body.items.iter() {
            if let LoweredItemKind::Assignment(assignment) = &item.kind
                && let Some(evidence) = &assignment.evidence
            {
                self.check_evidence(&item.origin, evidence);
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
        match evidence {
            TraitEvidence::ExplicitImplementation { arguments, .. }
            | TraitEvidence::Structural { arguments, .. } => {
                for argument in arguments {
                    if contains_type_parameter(argument)
                        || CanonicalType::concrete(argument, origin).is_err()
                    {
                        self.report(
                            origin,
                            format!("emitted evidence keeps a template-only argument: {argument}"),
                        );
                    }
                }
            }
            TraitEvidence::DeclaredBound { .. } | TraitEvidence::RejectedImplementation { .. } => {
                self.report(
                    origin,
                    "emitted instance retains an unresolved trait evidence recipe",
                )
            }
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
        | LoweredBindingSite::FormattingWrite(id)
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
    /// Test-only: the interned instance whose template, concrete callable
    /// type, and substitutions reproduce one legacy specialization. The
    /// legacy queue does not record evidence, so it is excluded here.
    pub(crate) fn instance_for_legacy_specialization(
        &self,
        function: crate::FunctionId,
        function_type: &crate::CheckedFunctionType,
        substitutions: &std::collections::HashMap<crate::TypeParameterId, crate::CheckedType>,
    ) -> Option<FunctionInstanceId> {
        self.instances.iter().find_map(|(id, instance)| {
            if instance.template != function
                || !instance
                    .body
                    .as_ref()
                    .is_some_and(|body| &body.signature == function_type)
            {
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

    use crate::specialization::{
        ArtifactRequestKey, CanonicalFunctionType, CanonicalType, GcFinalizerKey,
    };
    use crate::{
        CallTypeSubstitution, CheckedFunctionType, CheckedType, ConstructorConstruction, DebugStep,
        LoweredArtifactPlan, LoweredModule, Lowerer, NameResolver, PlannedArtifact, PlannedCallee,
        ProgramLoader, StructuralBody, StructuralTraitMethod, SubstitutionEnvironment, TypeChecker,
        TypeParameterId, TypedModule,
    };

    use super::super::{
        LoweredArtifactRequestId, LoweredArtifactRequestRoot, LoweredBindingSite,
        LoweredBoundTarget, LoweredCallStep, LoweredCallableCategory, LoweredCallableTarget,
        LoweredExpressionKind, LoweredInstanceDependency, LoweredInstanceDependencyKind,
        LoweredInstanceRequest, LoweredRepeatCount, LoweredStringTemplatePart, TraitEvidence,
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
                closure_phase: false,
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
    fn graph_validator_reports_template_only_evidence_arguments() {
        let mut program = lower(concat!(
            "trait TestShow T { test_show: T -> Bool }\n",
            "impl TestShow I32 { def test_show = _ => True }\n",
            "def show: <T where TestShow T> T -> Bool = value => test_show value\n",
            "let shown: Bool = show 1\n",
        ))
        .program;
        let id = program
            .instances
            .iter()
            .find(|(_, instance)| {
                instance
                    .body
                    .as_ref()
                    .is_some_and(|body| !body.evidence.is_empty())
            })
            .map(|(id, _)| id)
            .expect("a concrete trait selection in an instance body");
        let body = program
            .instances
            .get_mut(id)
            .and_then(|instance| instance.body.as_mut())
            .expect("instance body");
        let evidence = body.evidence.values_mut().next().expect("evidence entry");
        let TraitEvidence::ExplicitImplementation { arguments, .. } = evidence else {
            panic!("expected explicit implementation evidence");
        };
        arguments.push(crate::CheckedType::Parameter {
            id: TypeParameterId(999),
            name: "Unresolved".to_owned(),
            sized: true,
        });
        let diagnostics = program.validate_specialization_graph();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("template-only argument")),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn graph_validator_checks_indexed_assignment_evidence() {
        let mut program = lower(concat!(
            "def update: () -> () = () => { let mut values: (I32; 2) = (1, 2); values[0] = 3; () }\n",
            "let updated = update ()\n",
        ))
        .program;
        let id = program
            .instances
            .iter()
            .find(|(_, instance)| {
                instance.body.as_ref().is_some_and(|body| {
                    body.items.iter().any(|(_, item)| {
                        matches!(
                            &item.kind,
                            LoweredItemKind::Assignment(assignment)
                                if assignment.evidence.is_some()
                        )
                    })
                })
            })
            .map(|(id, _)| id)
            .expect("indexed assignment instance");
        let body = program
            .instances
            .get_mut(id)
            .and_then(|instance| instance.body.as_mut())
            .expect("instance body");
        let item_id = body
            .items
            .iter()
            .find(|(_, item)| {
                matches!(
                    &item.kind,
                    LoweredItemKind::Assignment(assignment) if assignment.evidence.is_some()
                )
            })
            .map(|(id, _)| id)
            .expect("indexed assignment item");
        let item = body
            .items
            .get_mut(item_id)
            .expect("indexed assignment item");
        let LoweredItemKind::Assignment(assignment) = &mut item.kind else {
            unreachable!();
        };
        let evidence = assignment.evidence.as_mut().expect("assignment evidence");
        let TraitEvidence::Structural { arguments, .. } = evidence else {
            panic!("expected structural assignment evidence");
        };
        arguments.push(crate::CheckedType::Parameter {
            id: TypeParameterId(999),
            name: "Unresolved".to_owned(),
            sized: true,
        });
        let diagnostics = program.validate_specialization_graph();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("template-only argument")),
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
                // Stage 4.4 adds cleanup artifacts over the same catalog.
                Some(ArtifactRequestKey::DropGlue(_))
                | Some(ArtifactRequestKey::GcFinalizer(_)) => {}
                Some(other) => panic!("unexpected artifact family `{}`", other.family_name()),
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

    /// Stage 4.3's first claim: every constructor-value and structural-method
    /// site in a materialized instance body is already bound to its artifact
    /// by the Stage 3.3 worklist and Stage 3.4 binder, and every constructor
    /// or structural artifact is a Stage 3 request root. No 4.3 scanner or
    /// use-site variant is needed.
    #[test]
    fn constructor_and_structural_sites_need_no_stage_4_3_scanner() {
        let lowered = lower(concat!(
            "type Point = ctor (I32, I32)\n",
            "let make: () -> ((I32, I32) -> Point) = () => Point\n",
            "def debug_pair: (I32, I32) -> String = pair => \"${pair:?}\"\n",
            "def index_pair: (I32, I32) -> I32 = pair => pair[0]\n",
            "def mutate_pair: (I32, I32) -> (I32, I32) = pair => {\n",
            "  let mut copy = pair\n",
            "  copy[0] = 3\n",
            "  copy\n",
            "}\n",
            "def count_pair: (I32, I32) -> I32 = pair => {\n",
            "  let mut total = 0\n",
            "  for item in pair { total = total + item }\n",
            "  total\n",
            "}\n",
            "let text = debug_pair (1, 2)\n",
            "let element = index_pair (1, 2)\n",
            "let mutated = mutate_pair (1, 2)\n",
            "let total = count_pair (1, 2)\n",
        ));
        let program = &lowered.program;
        let mut constructor_sites = 0;
        let mut structural_sites = 0;
        for (_, instance) in program.instances.iter() {
            let Some(body) = &instance.body else {
                continue;
            };
            for (site, target) in &body.bindings {
                if let LoweredBindingSite::CallableValue(id) = site
                    && let Some(value) = body.callable_value(*id)
                    && matches!(value.target, LoweredCallableTarget::Constructor { .. })
                {
                    assert!(
                        matches!(target, LoweredBoundTarget::Artifact(_)),
                        "a constructor-value site binds its adapter artifact"
                    );
                    constructor_sites += 1;
                }
                if let Some(TraitEvidence::Structural { .. }) = body.resolved_evidence(*site) {
                    assert!(
                        matches!(target, LoweredBoundTarget::Artifact(_)),
                        "a structural selection binds its structural artifact"
                    );
                    structural_sites += 1;
                }
            }
        }
        assert!(
            constructor_sites > 0,
            "the fixture uses a constructor value"
        );
        assert!(
            structural_sites >= 4,
            "the fixture exercises Debug/Index/MutateIndex/IntoIterator/Iterator: {structural_sites}"
        );

        // Every constructor and structural artifact entered as a Stage 3
        // request root (from an initializer or an instance body), never as a
        // closure-phase discovery, so 4.3 registers no scanner.
        let mut roots = 0;
        for (_, artifact) in program.artifacts.iter() {
            let Some(key) = program.specializations.artifact(artifact.ordinal) else {
                continue;
            };
            if !matches!(
                key,
                ArtifactRequestKey::ConstructorAdapter(_) | ArtifactRequestKey::StructuralMethod(_)
            ) {
                continue;
            }
            assert!(
                matches!(
                    artifact.request,
                    LoweredArtifactRequestRoot::Instance { .. }
                        | LoweredArtifactRequestRoot::Initializer { .. }
                ),
                "artifact `{}` is not a Stage 3 request root",
                key.family_name()
            );
            roots += 1;
        }
        assert!(
            roots > 0,
            "the fixture reserves constructor/structural artifacts"
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
        eprintln!("stage 4.4 fixture:\n{source}");
        let legacy =
            crate::codegen::legacy_emissions(&context, &lowered).unwrap_or_else(|diagnostics| {
                panic!("the legacy backend should compile the module: {diagnostics:?}\n{source}")
            });
        let program = &lowered.program;
        for (function, function_type, substitutions) in &legacy.specializations {
            let name = program
                .functions
                .get(*function)
                .map(|template| template.name.as_str())
                .unwrap_or("<missing>");
            assert!(
                program
                    .instance_for_legacy_specialization(*function, function_type, substitutions)
                    .is_some(),
                "legacy specialization for `{name}` has no matching instance"
            );
        }
        let origin = Origin::compiler();
        for adapter in &legacy.constructor_adapters {
            let callable_type = CanonicalFunctionType::concrete(&adapter.callable_type, &origin)
                .expect("a legacy adapter type is concrete");
            let found = program.artifacts.iter().any(|(_, artifact)| {
                matches!(
                    program.specializations.artifact(artifact.ordinal),
                    Some(ArtifactRequestKey::ConstructorAdapter(key))
                        if key.symbol == adapter.symbol && key.callable_type == callable_type
                )
            });
            assert!(
                found,
                "legacy constructor adapter for symbol {} has no artifact request",
                adapter.symbol.0
            );
        }
        for method in &legacy.structural_methods {
            let arguments = method
                .arguments
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
                        if key.structural == method.structural && key.arguments == arguments
                )
            });
            assert!(
                found,
                "legacy structural method {:?} has no artifact request",
                method.structural
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
                "use std.cinterop.(CString, c_string)\n",
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

    // ------------------------------------------------------------------
    // Stage 4.4 cleanup transition comparison.
    // ------------------------------------------------------------------

    /// The observed cleanup coverage of one transition fixture.
    #[derive(Debug, Default)]
    struct CleanupCoverage {
        drop_kinds: std::collections::HashSet<&'static str>,
        finalizer_kinds: std::collections::HashSet<&'static str>,
        use_sites: std::collections::HashSet<&'static str>,
        owned: usize,
        buffer_clones: usize,
    }

    impl CleanupCoverage {
        fn merge(&mut self, other: CleanupCoverage) {
            self.drop_kinds.extend(other.drop_kinds);
            self.finalizer_kinds.extend(other.finalizer_kinds);
            self.use_sites.extend(other.use_sites);
            self.owned += other.owned;
            self.buffer_clones += other.buffer_clones;
        }
    }

    fn drop_branch_name(branch: &crate::codegen::LegacyDropBranch) -> &'static str {
        use crate::codegen::LegacyDropBranch;
        match branch {
            LegacyDropBranch::CoroutineCleanup => "coroutine-cleanup",
            LegacyDropBranch::RuntimeRelease(name) => name,
            LegacyDropBranch::CStringFree => "cstring-free",
            LegacyDropBranch::Product => "product",
            LegacyDropBranch::Sum => "sum",
            LegacyDropBranch::Distinct => "distinct",
            LegacyDropBranch::UserDrop(_) => "user-drop",
            LegacyDropBranch::NoOp => "no-op",
        }
    }

    fn drop_glue_plan_for<'a>(
        program: &'a LoweredProgram,
        value_type: &crate::CheckedType,
    ) -> Option<&'a crate::DropGluePlan> {
        let canonical = CanonicalType::concrete(value_type, &Origin::compiler()).ok()?;
        let ordinal = program
            .specializations
            .artifact_ordinal(&ArtifactRequestKey::DropGlue(canonical))?;
        program
            .artifacts
            .iter()
            .find(|(_, artifact)| artifact.ordinal == ordinal)
            .and_then(|(_, artifact)| artifact.plan.as_ref())
            .and_then(|plan| match plan {
                crate::LoweredArtifactPlan::DropGlue(plan) => Some(plan),
                _ => None,
            })
    }

    fn nested_glue_plan<'a>(
        program: &'a LoweredProgram,
        glue: &PlannedArtifact,
    ) -> &'a crate::DropGluePlan {
        let ordinal = glue.artifact.expect("bound after closure");
        let artifact = program
            .artifacts
            .iter()
            .find(|(_, artifact)| artifact.ordinal == ordinal)
            .map(|(_, artifact)| artifact)
            .expect("nested glue artifact");
        match artifact.plan.as_ref().expect("expanded") {
            crate::LoweredArtifactPlan::DropGlue(plan) => plan,
            other => panic!("expected a nested drop-glue plan, got {other:?}"),
        }
    }

    /// Requires one legacy drop call tree to match its plan: same branch, the
    /// same user-drop function, and the same nested cleanups in order. Legacy
    /// records the no-op nested calls the design drops by equivalence.
    fn assert_drop_call_matches(
        program: &LoweredProgram,
        plan: &crate::DropGluePlan,
        call: &crate::codegen::LegacyDropCall,
    ) {
        use crate::codegen::LegacyDropBranch;
        use crate::{DropGlueBody, RuntimeRelease};
        let nested = call
            .nested
            .iter()
            .filter(|nested| !matches!(nested.branch, LegacyDropBranch::NoOp))
            .collect::<Vec<_>>();
        match (&plan.body, &call.branch) {
            (
                DropGlueBody::UserDrop {
                    method,
                    representation,
                },
                LegacyDropBranch::UserDrop(function),
            ) => {
                let bound = program
                    .instances
                    .get(method.instance.expect("bound after closure"))
                    .expect("method instance");
                assert_eq!(bound.template, *function, "the drop method function");
                match (representation, nested.as_slice()) {
                    (Some(glue), [inner]) => {
                        assert_drop_call_matches(program, nested_glue_plan(program, glue), inner)
                    }
                    (None, []) => {}
                    (None, inner) => panic!(
                        "the plan omits a legacy representation drop for `{}`: {inner:?}",
                        call.value_type
                    ),
                    (Some(_), []) => panic!(
                        "the plan adds a representation drop legacy never calls for `{}`",
                        call.value_type
                    ),
                    (Some(_), _) => panic!("a user drop has one representation drop"),
                }
            }
            (DropGlueBody::CoroutineCleanup, LegacyDropBranch::CoroutineCleanup) => {
                assert!(nested.is_empty())
            }
            (DropGlueBody::RuntimeRelease(planned), LegacyDropBranch::RuntimeRelease(name)) => {
                let expected = match planned {
                    RuntimeRelease::SchedulerDestroy => "__staple_sched_destroy",
                    RuntimeRelease::WaitDrop => "__staple_completion_wait_drop",
                    RuntimeRelease::ResolverDrop => "__staple_completion_resolver_drop",
                    RuntimeRelease::CompletionTokenRelease => "__staple_completion_token_release",
                };
                assert_eq!(*name, expected, "the runtime release function");
                assert!(nested.is_empty());
            }
            (DropGlueBody::CStringFree, LegacyDropBranch::CStringFree) => {
                assert!(nested.is_empty())
            }
            (DropGlueBody::Product { fields }, LegacyDropBranch::Product) => {
                assert_eq!(
                    fields.len(),
                    nested.len(),
                    "the plan lists the same droppable fields for `{}`",
                    call.value_type
                );
                for (field, inner) in fields.iter().zip(&nested) {
                    assert_eq!(field.value_type, inner.value_type);
                    assert_drop_call_matches(
                        program,
                        nested_glue_plan(program, &field.glue),
                        inner,
                    );
                }
            }
            (DropGlueBody::Sum { alternatives }, LegacyDropBranch::Sum) => {
                assert_eq!(
                    alternatives.len(),
                    nested.len(),
                    "the plan lists the same droppable alternatives for `{}`",
                    call.value_type
                );
                for (alternative, inner) in alternatives.iter().zip(&nested) {
                    assert_eq!(alternative.value_type, inner.value_type);
                    assert_drop_call_matches(
                        program,
                        nested_glue_plan(program, &alternative.glue),
                        inner,
                    );
                }
            }
            (DropGlueBody::Distinct { representation }, LegacyDropBranch::Distinct) => {
                match nested.as_slice() {
                    [inner] => assert_drop_call_matches(
                        program,
                        nested_glue_plan(program, representation),
                        inner,
                    ),
                    _ => panic!(
                        "the distinct branch has exactly one representation drop for `{}`",
                        call.value_type
                    ),
                }
            }
            (DropGlueBody::Unexpanded, _) => panic!("the drop glue was never expanded"),
            (body, branch) => panic!(
                "drop branch mismatch for `{}`: plan {body:?} vs legacy {branch:?}",
                call.value_type
            ),
        }
    }

    /// One fixture's cleanup transition comparison: drop glue, finalizers,
    /// owned bindings, and buffer-clone selections all agree with legacy
    /// emission.
    fn assert_cleanup_matches_legacy(source: &str) -> CleanupCoverage {
        use crate::GcFinalizerPlan;
        use crate::codegen::{LegacyDropBranch, LegacyFinalizer};
        let mut coverage = CleanupCoverage::default();
        let module = checked_program(source);
        let lowered = Lowerer::new().lower(&module).unwrap_or_else(|diagnostics| {
            panic!("source should lower through the production closure: {diagnostics:?}\n{source}")
        });
        let context = Context::create();
        let legacy =
            crate::codegen::legacy_emissions(&context, &lowered).unwrap_or_else(|diagnostics| {
                panic!("the legacy backend should compile the module: {diagnostics:?}\n{source}")
            });
        let program = &lowered.program;
        let origin = Origin::compiler();

        // Drop glue: the legacy type set equals the plan key set, and every
        // legacy call tree matches its plan branch, function, and nested order.
        // The legacy set walks every call tree, because a type dropped only
        // inside another type's glue is still a `DropGlue` key.
        let mut legacy_types = std::collections::HashSet::new();
        for call in &legacy.drop_calls {
            assert!(
                !matches!(call.branch, LegacyDropBranch::NoOp),
                "legacy only drops droppable values: {:?}",
                call.value_type
            );
        }
        let mut pending = legacy.drop_calls.iter().collect::<Vec<_>>();
        while let Some(call) = pending.pop() {
            // Nested no-op calls are the representation drops the plan omits
            // by equivalence; they name no glue.
            if matches!(call.branch, LegacyDropBranch::NoOp) {
                continue;
            }
            legacy_types.insert(
                CanonicalType::concrete(&call.value_type, &origin).expect("concrete drop type"),
            );
            pending.extend(&call.nested);
        }
        let mut plan_types = std::collections::HashSet::new();
        for (_, artifact) in program.artifacts.iter() {
            let Some(crate::LoweredArtifactPlan::DropGlue(plan)) = &artifact.plan else {
                continue;
            };
            plan_types.insert(
                CanonicalType::concrete(&plan.value_type, &origin).expect("concrete plan type"),
            );
        }
        assert_eq!(
            legacy_types, plan_types,
            "the legacy drop types equal the drop-glue keys"
        );
        for call in &legacy.drop_calls {
            let plan = drop_glue_plan_for(program, &call.value_type)
                .unwrap_or_else(|| panic!("no drop-glue plan for `{}`", call.value_type));
            coverage.drop_kinds.insert(drop_branch_name(&call.branch));
            assert_drop_call_matches(program, plan, call);
        }

        // Finalizers: every legacy finalizer matches exactly one plan and vice
        // versa, with closure finalizers mapped through the closure instance.
        let mut matched = std::collections::HashSet::new();
        for finalizer in &legacy.finalizers {
            match finalizer {
                LegacyFinalizer::Payload(value_type) => {
                    assert!(
                        matched.insert(format!("payload:{:?}", canonical(value_type))),
                        "legacy creates one payload finalizer per key"
                    );
                    let _ = assert_finalizer_plan(
                        program,
                        GcFinalizerKey::Payload(canonical(value_type)),
                    );
                }
                LegacyFinalizer::Cell(value_type) => {
                    assert!(matched.insert(format!("cell:{:?}", canonical(value_type))));
                    let _ =
                        assert_finalizer_plan(program, GcFinalizerKey::Cell(canonical(value_type)));
                }
                LegacyFinalizer::Buffer(element) => {
                    assert!(matched.insert(format!("buffer:{:?}", canonical(element))));
                    let _ =
                        assert_finalizer_plan(program, GcFinalizerKey::Buffer(canonical(element)));
                }
                LegacyFinalizer::ClosureEnvironment {
                    function,
                    capture_types,
                    dropped,
                } => {
                    let expected = capture_types
                        .iter()
                        .map(|capture| canonical(capture))
                        .collect::<Vec<_>>();
                    let instance = program
                        .instances
                        .iter()
                        .find(|(_, instance)| {
                            if instance.template != *function {
                                return false;
                            }
                            let Some(body) = &instance.body else {
                                return false;
                            };
                            body.captures()
                                .iter()
                                .map(|capture| canonical(&capture.value_type))
                                .collect::<Vec<_>>()
                                == expected
                        })
                        .map(|(id, _)| id)
                        .unwrap_or_else(|| {
                            panic!("no closure instance matches legacy finalizer {function:?}")
                        });
                    let ordinal = program
                        .instances
                        .get(instance)
                        .expect("closure instance")
                        .ordinal;
                    matched.insert(format!("closure:{}:{expected:?}", ordinal.index()));
                    let key = GcFinalizerKey::ClosureEnvironment {
                        closure: ordinal,
                        captures: expected,
                    };
                    let plan = assert_finalizer_plan(program, key);
                    let GcFinalizerPlan::ClosureEnvironment { drops, .. } = plan else {
                        panic!("expected a closure-environment plan")
                    };
                    let planned = drops.as_ref().expect("expanded");
                    assert_eq!(
                        planned.len(),
                        dropped.len(),
                        "the planned capture drop count matches legacy"
                    );
                    for (record, index) in planned.iter().zip(dropped) {
                        assert_eq!(record.index, *index, "the dropped capture index");
                        assert_eq!(
                            canonical(&record.value_type),
                            canonical(&capture_types[*index]),
                            "the dropped capture type"
                        );
                    }
                }
            }
            coverage.finalizer_kinds.insert(match finalizer {
                LegacyFinalizer::Payload(_) => "payload",
                LegacyFinalizer::Cell(_) => "cell",
                LegacyFinalizer::ClosureEnvironment { .. } => "closure",
                LegacyFinalizer::Buffer(_) => "buffer",
            });
        }
        for (_, artifact) in program.artifacts.iter() {
            let Some(crate::LoweredArtifactPlan::GcFinalizer(plan)) = &artifact.plan else {
                continue;
            };
            let key = program
                .specializations
                .artifact(artifact.ordinal)
                .expect("finalizer key");
            assert!(
                matched.contains(&legacy_finalizer_name(key)),
                "every finalizer plan matches a legacy finalizer: {key:?}"
            );
            match plan {
                GcFinalizerPlan::Payload { glue, .. }
                | GcFinalizerPlan::Cell { glue, .. }
                | GcFinalizerPlan::Buffer { glue, .. } => {
                    assert!(glue.is_some(), "the finalizer references its glue");
                }
                GcFinalizerPlan::ClosureEnvironment { drops, .. } => {
                    assert!(drops.is_some(), "the closure finalizer is expanded");
                }
            }
        }

        // Owned bindings: per emitted function, legacy registrations match the
        // instance's records in order and storage kind.
        let mut groups: std::collections::BTreeMap<usize, Vec<&crate::codegen::LegacyOwned>> =
            std::collections::BTreeMap::new();
        for owned in &legacy.owned {
            let instance = program
                .instance_for_legacy_specialization(
                    owned.function,
                    &owned.function_type,
                    &owned.substitutions,
                )
                .unwrap_or_else(|| {
                    panic!(
                        "no instance for legacy owned registrations in {:?}",
                        owned.function
                    )
                });
            groups.entry(instance.index()).or_default().push(owned);
        }
        for (instance_index, owned) in &groups {
            let instance = FunctionInstanceId::from_index(*instance_index);
            let body = program
                .instances
                .get(instance)
                .and_then(|instance| instance.body.as_ref())
                .expect("instance body");
            coverage.owned += owned.len();
            let expected = owned
                .iter()
                .map(|owned| (owned.symbol, owned.cell))
                .collect::<Vec<_>>();
            let actual = body
                .owned_bindings
                .iter()
                .map(|record| (record.symbol, record.storage == crate::OwnedStorage::Cell))
                .collect::<Vec<_>>();
            assert_eq!(
                actual, expected,
                "instance {} owned registrations and storage kinds",
                instance_index
            );
        }

        // Buffer clones: each legacy selection matches an element instance use.
        let mut clone_uses = Vec::new();
        for (_, instance) in program.instances.iter() {
            let Some(body) = &instance.body else {
                continue;
            };
            for use_ in &body.instance_uses {
                if let crate::ArtifactUseSite::BufferCloneElement(call) = use_.site {
                    let element = match &body.call(call).expect("clone call").result_type {
                        crate::CheckedType::Buffer(element) => element.as_ref().clone(),
                        other => panic!("buffer clone over a non-buffer: {other:?}"),
                    };
                    let bound = program
                        .instances
                        .get(use_.instance)
                        .expect("clone instance")
                        .template;
                    clone_uses.push((element, bound));
                }
            }
        }
        let mut legacy_clones = legacy
            .buffer_clones
            .iter()
            .map(|clone| (clone.element.clone(), clone.function))
            .collect::<Vec<_>>();
        assert_eq!(
            clone_uses.len(),
            legacy_clones.len(),
            "each legacy buffer clone matches exactly one planned instance use"
        );
        while let Some(use_) = clone_uses.pop() {
            let position = legacy_clones
                .iter()
                .position(|clone| canonical(&clone.0) == canonical(&use_.0) && clone.1 == use_.1)
                .unwrap_or_else(|| panic!("no legacy buffer clone matches {use_:?}"));
            legacy_clones.remove(position);
            coverage.buffer_clones += 1;
        }
        for (_, instance) in program.instances.iter() {
            if let Some(body) = &instance.body {
                for use_ in &body.instance_uses {
                    coverage.use_sites.insert(use_site_name(use_.site));
                }
                for use_ in &body.artifact_uses {
                    coverage.use_sites.insert(use_site_name(use_.site));
                }
            }
        }
        for uses in &program.initializer_artifact_uses {
            for use_ in uses {
                coverage.use_sites.insert(use_site_name(use_.site));
            }
        }
        for uses in &program.initializer_instance_uses {
            for use_ in uses {
                coverage.use_sites.insert(use_site_name(use_.site));
            }
        }
        coverage
    }

    fn canonical(value_type: &crate::CheckedType) -> CanonicalType {
        CanonicalType::concrete(value_type, &Origin::compiler()).expect("a concrete type")
    }

    fn assert_finalizer_plan(
        program: &LoweredProgram,
        key: GcFinalizerKey,
    ) -> &crate::GcFinalizerPlan {
        let ordinal = program
            .specializations
            .artifact_ordinal(&ArtifactRequestKey::GcFinalizer(key))
            .unwrap_or_else(|| panic!("no finalizer plan matches the legacy finalizer"));
        program
            .artifacts
            .iter()
            .find(|(_, artifact)| artifact.ordinal == ordinal)
            .and_then(|(_, artifact)| artifact.plan.as_ref())
            .and_then(|plan| match plan {
                crate::LoweredArtifactPlan::GcFinalizer(plan) => Some(plan),
                _ => None,
            })
            .expect("the finalizer plan is expanded")
    }

    /// The `matched` set key name for one finalizer key, mirroring the legacy
    /// finalizer names so plan/finalizer matching is bidirectional.
    fn legacy_finalizer_name(key: &ArtifactRequestKey) -> String {
        match key {
            ArtifactRequestKey::GcFinalizer(GcFinalizerKey::Payload(value_type)) => {
                format!("payload:{value_type:?}")
            }
            ArtifactRequestKey::GcFinalizer(GcFinalizerKey::Cell(value_type)) => {
                format!("cell:{value_type:?}")
            }
            ArtifactRequestKey::GcFinalizer(GcFinalizerKey::Buffer(element)) => {
                format!("buffer:{element:?}")
            }
            ArtifactRequestKey::GcFinalizer(GcFinalizerKey::ClosureEnvironment {
                closure,
                captures,
            }) => format!("closure:{}:{captures:?}", closure.index()),
            _ => String::new(),
        }
    }

    fn use_site_name(site: crate::ArtifactUseSite) -> &'static str {
        use crate::ArtifactUseSite;
        match site {
            ArtifactUseSite::DiscardedResult(_) => "discarded",
            ArtifactUseSite::ReplacedValue(_) => "replaced",
            ArtifactUseSite::LoopBodyResult(_) => "loop-body",
            ArtifactUseSite::CallTemporary { .. } => "call-temporary",
            ArtifactUseSite::CStringTemporary(_) => "cstring-temporary",
            ArtifactUseSite::WildcardDiscard(_) => "wildcard",
            ArtifactUseSite::OwnedBinding(_) => "owned",
            ArtifactUseSite::CellFinalizer(_) => "cell",
            ArtifactUseSite::ClosureEnvironment(_) => "closure",
            ArtifactUseSite::RefConstruction(_) => "ref",
            ArtifactUseSite::DropIntrinsic(_) => "drop-intrinsic",
            ArtifactUseSite::CStringConversion(_) => "cstring-conversion",
            ArtifactUseSite::CompletionOrphan(_) => "completion-orphan",
            ArtifactUseSite::BufferAllocation(_) => "buffer-allocation",
            ArtifactUseSite::BufferCloneFinalizer(_) => "buffer-clone-finalizer",
            ArtifactUseSite::BufferCloneElement(_) => "buffer-clone-element",
            #[cfg(test)]
            ArtifactUseSite::Test(_) => "test",
        }
    }

    #[test]
    fn stage_4_4_cleanup_matches_legacy_emission() {
        let mut coverage = CleanupCoverage::default();
        for source in [
            concat!(
                "use std.cinterop.(CString, c_string)\n",
                "extern \"c\" { inspect: CString -> I32 }\n",
                "type Resource = ctor I32\n",
                "impl Drop Resource { def drop = Resource value => () }\n",
                "type Handle = ctor CString\n",
                "impl Drop Handle { def drop = Handle value => () }\n",
                "type Wrapped = ctor CString\n",
                "def make_c: () -> CString = () => c_string \"x\"\n",
                "def discard_c: () -> () = () => { make_c (); () }\n",
                "def extern_temp: () -> I32 = () => inspect (c_string \"x\")\n",
                "def consume_mut: mut CString -> () = mut value => () \n",
                "def call_temp: () -> () = () => { consume_mut (make_c ()); () }\n",
                "def drop_value: () -> () = () => { drop (make_c ()); () }\n",
                "def mutate_resource: move (Resource, Resource) -> (Resource, Resource) = move pair => {\n",
                "  let mut copy = pair\n",
                "  copy[0] = Resource 3\n",
                "  copy\n",
                "}\n",
                "def mutate_handle: move (Handle, Handle) -> (Handle, Handle) = move pair => {\n",
                "  let mut copy = pair\n",
                "  copy[0] = Handle (c_string \"b\")\n",
                "  copy\n",
                "}\n",
                "def mutate_wrapped: move (Wrapped, Wrapped) -> (Wrapped, Wrapped) = move pair => {\n",
                "  let mut copy = pair\n",
                "  copy[0] = Wrapped (c_string \"b\")\n",
                "  copy\n",
                "}\n",
                "def mutate_c: move (CString, CString) -> (CString, CString) = move pair => {\n",
                "  let mut copy = pair\n",
                "  copy[0] = c_string \"b\"\n",
                "  copy\n",
                "}\n",
                "def mutate_product: move ((I32, CString), (I32, CString)) -> ((I32, CString), (I32, CString)) = move pair => {\n",
                "  let mut copy = pair\n",
                "  copy[0] = (1, c_string \"b\")\n",
                "  copy\n",
                "}\n",
                "def pick: Bool -> (CString | I32) = condition => when { condition => c_string \"a\", else => 1 }\n",
                "def mutate_sum: move ((CString | I32), (CString | I32)) -> ((CString | I32), (CString | I32)) = move pair => {\n",
                "  let mut copy = pair\n",
                "  copy[0] = pick True\n",
                "  copy\n",
                "}\n",
                "let a = mutate_resource (Resource 1, Resource 2)\n",
                "let b = mutate_handle (Handle (c_string \"a\"), Handle (c_string \"c\"))\n",
                "let c = mutate_wrapped (Wrapped (c_string \"a\"), Wrapped (c_string \"c\"))\n",
                "let d = mutate_c (c_string \"a\", c_string \"c\")\n",
                "let e = mutate_product ((1, c_string \"a\"), (2, c_string \"c\"))\n",
                "let f = mutate_sum (pick False, pick True)\n",
                "let g = discard_c ()\n",
                "let h = extern_temp ()\n",
                "let i = call_temp ()\n",
                "let j = drop_value ()\n",
            ),
            concat!(
                "use std.cinterop.(CString, c_string)\n",
                "use std.coroutine.*\n",
                "def task: () -> Coroutine{} I32 = () => coro { 1 }\n",
                "def discard_task: () -> () = () => { task (); () }\n",
                "def make_c: () -> CString = () => c_string \"x\"\n",
                "def discard_scheduler: () -> () = () => { scheduler (); () }\n",
                "def make_completion: () -> (wait: Wait I32, resolver: Resolver I32) = () => completion (scheduler ())\n",
                "def discard_completion: () -> () = () => { make_completion (); () }\n",
                "def make_token: () -> (wait: Wait (), token: CompletionToken) = () => completion_token (scheduler ())\n",
                "def discard_token: () -> () = () => { make_token (); () }\n",
                "def make_resolver: () -> (wait: Wait CString, resolver: Resolver CString) = () => completion (scheduler ())\n",
                "def discard_orphan: () -> () = () => {\n",
                "  let pending = make_resolver ()\n",
                "  Resolver.complete (pending.resolver) (make_c ())\n",
                "  ()\n",
                "}\n",
                "let a = discard_task ()\n",
                "let b = discard_scheduler ()\n",
                "let c = discard_completion ()\n",
                "let d = discard_token ()\n",
                "let e = discard_orphan ()\n",
            ),
            concat!(
                "use std.buffer.*\n",
                "use std.clone.Clone\n",
                "use std.cinterop.*\n",
                "extern \"c\" { inspect: CString -> I32 }\n",
                "def make_ref: () -> Ref CString = () => Ref (c_string \"x\")\n",
                "def cell_finalizer: () -> (() -> I32) = () => {\n",
                "  let mut cell = c_string \"a\"\n",
                "  cell = c_string \"b\"\n",
                "  () => inspect cell\n",
                "}\n",
                "def closure_env: move CString -> (() -> I32) = move value => () => inspect value\n",
                "type Owned = ctor I32\n",
                "impl Drop Owned { def drop = Owned value => () }\n",
                "impl Clone Owned { def clone = Owned value => Owned value }\n",
                "def make_buffer: () -> Buffer Owned = () => Buffer.with_capacity 2\n",
                "def clone_owned: (Buffer Owned) -> Buffer Owned = buffer => Clone.clone buffer\n",
                "def clone_copy: (Buffer I32) -> Buffer I32 = buffer => Clone.clone buffer\n",
                "let a = make_ref ()\n",
                "let b = cell_finalizer ()\n",
                "let c = closure_env (c_string \"e\")\n",
                "let d = make_buffer ()\n",
            ),
            concat!(
                "use std.cinterop.*\n",
                "extern \"c\" { inspect: CString -> I32 }\n",
                "def owned_param: move CString -> CString = move value => value\n",
                "def mutate_pair: move (CString, CString) -> (CString, CString) = move pair => {\n",
                "  let mut copy = pair\n",
                "  copy[0] = c_string \"b\"\n",
                "  copy\n",
                "}\n",
                "def nested: (I32) -> I32 = value => {\n",
                "  let outer = c_string \"a\"\n",
                "  when { value > 0 => { let inner = c_string \"b\"; inspect inner }, else => inspect outer }\n",
                "}\n",
            ),
            // `(CString | I32)` is dropped only inside the product's glue,
            // never as a root, so the legacy type set must walk nested calls.
            concat!(
                "use std.cinterop.(CString, c_string)\n",
                "def make: () -> (I32, (CString | I32)) = () => (1, 5)\n",
                "def run: () -> () = () => { let p = make (); () }\n",
                "let e = run ()\n",
            ),
        ] {
            coverage.merge(assert_cleanup_matches_legacy(source));
        }
        // `WaitDrop` and `ResolverDrop` are covered by the hook-based
        // `drop_glue_bodies_mirror_the_legacy_decision_order` fixture, where
        // the opaque types are requested directly; this program only makes
        // scheduler and completion-token values droppable.
        for kind in [
            "user-drop",
            "coroutine-cleanup",
            "__staple_sched_destroy",
            "__staple_completion_token_release",
            "cstring-free",
            "product",
            "sum",
            "distinct",
        ] {
            assert!(
                coverage.drop_kinds.contains(kind),
                "the fixtures cover the `{kind}` drop branch: {coverage:?}"
            );
        }
        for kind in ["payload", "cell", "closure", "buffer"] {
            assert!(
                coverage.finalizer_kinds.contains(kind),
                "the fixtures cover the `{kind}` finalizer: {coverage:?}"
            );
        }
        // `loop-body` is covered by the scanner fixture; a `Never`-valued loop
        // body trips a pre-existing legacy emission error ("cannot generate
        // code for an erroneous type"), so the transition comparison cannot
        // include it.
        for site in [
            "discarded",
            "replaced",
            "call-temporary",
            "cstring-temporary",
            "wildcard",
            "owned",
            "cell",
            "closure",
            "ref",
            "drop-intrinsic",
            "cstring-conversion",
            "completion-orphan",
            "buffer-allocation",
            "buffer-clone-finalizer",
            "buffer-clone-element",
        ] {
            assert!(
                coverage.use_sites.contains(site),
                "the fixtures cover the `{site}` use site: {coverage:?}"
            );
        }
        assert!(coverage.owned >= 4, "owned registrations are compared");
        assert!(
            coverage.buffer_clones >= 2,
            "buffer clone selections are compared"
        );
    }

    #[test]
    fn stage_4_4_cleanup_matches_legacy_on_standard_library_values() {
        let source = concat!(
            "use std.list.*\n",
            "use std.buffer.*\n",
            "use std.coroutine.*\n",
            "def build: () -> List I32 = () => {\n",
            "  let mut items: List I32 = List.with_capacity 4\n",
            "  List.push (items) (1)\n",
            "  List.push (items) (2)\n",
            "  items\n",
            "}\n",
            "def task: () -> Coroutine{} I32 = () => coro { 1 }\n",
            "def discard_task: () -> () = () => { task (); () }\n",
            "def show_pair: (I32, I32) -> String = pair => \"${pair:?}\"\n",
            "let items = build ()\n",
            "let size = List.length items\n",
            "let text = show_pair (1, 2)\n",
            "let created = task ()\n",
            "let dropped = discard_task ()\n",
        );
        let coverage = assert_cleanup_matches_legacy(source);
        assert!(
            coverage.drop_kinds.contains("coroutine-cleanup")
                && coverage.drop_kinds.contains("__staple_sched_destroy"),
            "the standard-library program drops coroutine and scheduler values: {coverage:?}"
        );
        assert!(
            coverage.use_sites.contains("discarded"),
            "the standard-library program discards droppable results: {coverage:?}"
        );
        eprintln!("stage 4.4 stdlib cleanup coverage: {coverage:?}");
    }

    // ------------------------------------------------------------------
    // Stage 4.4 drop-glue transition comparison.
    // ------------------------------------------------------------------

    /// Every naturally requested `DropGlue` plan agrees with the typed module:
    /// the key's type needs drop, a user-drop body selects exactly
    /// `drop_method_for`, and every non-user body is planned only when no user
    /// implementation matches.
    fn assert_drop_glue_plans_agree(source: &str) -> (usize, crate::ClosureStats) {
        let module = checked_program(source);
        let lowered = Lowerer::new()
            .lower(&module)
            .expect("source should lower through the production closure");
        let program = &lowered.program;
        let stats = program
            .closure_stats
            .expect("the production closure records its stats");
        let mut plans = 0;
        for (_, artifact) in program.artifacts.iter() {
            let Some(crate::LoweredArtifactPlan::DropGlue(plan)) = &artifact.plan else {
                continue;
            };
            plans += 1;
            assert!(
                module.type_needs_drop(&plan.value_type),
                "drop glue `{}` is only requested for a droppable type",
                plan.value_type
            );
            match &plan.body {
                crate::DropGlueBody::Unexpanded => {
                    panic!("drop glue `{}` was never expanded", plan.value_type)
                }
                crate::DropGlueBody::UserDrop { method, .. } => {
                    let expected = module.drop_method_for(&plan.value_type).unwrap_or_else(|| {
                        panic!(
                            "a planned user drop for `{}` has no legacy selection",
                            plan.value_type
                        )
                    });
                    let bound = program
                        .instances
                        .get(method.instance.expect("bound after closure"))
                        .expect("method instance");
                    assert_eq!(bound.template, expected);
                    assert_eq!(
                        method.kind,
                        LoweredInstanceDependencyKind::DropMethod,
                        "the user-drop edge uses the drop-method kind"
                    );
                }
                _ => assert!(
                    module.drop_method_for(&plan.value_type).is_none(),
                    "a non-user body is planned for `{}` although a user implementation matches",
                    plan.value_type
                ),
            }
        }
        (plans, stats)
    }

    #[test]
    fn drop_glue_plans_agree_with_the_typed_module() {
        let mut plans = 0;
        let mut max_rounds = 0;
        let mut max_growth = 0;
        for source in [
            concat!(
                "type Resource = ctor I32\n",
                "impl Drop Resource { def drop = Resource value => () }\n",
                "def mutate_resource: move (Resource, Resource) -> (Resource, Resource) = move pair => {\n",
                "  let mut copy = pair\n",
                "  copy[0] = Resource 3\n",
                "  copy\n",
                "}\n",
                "let replaced = mutate_resource (Resource 1, Resource 2)\n",
            ),
            concat!(
                "use std.cinterop.(CString, c_string)\n",
                "type Handle = ctor CString\n",
                "impl Drop Handle { def drop = Handle value => () }\n",
                "type Wrapped = ctor CString\n",
                "def mutate_handle: move (Handle, Handle) -> (Handle, Handle) = move pair => {\n",
                "  let mut copy = pair\n",
                "  copy[0] = Handle (c_string \"b\")\n",
                "  copy\n",
                "}\n",
                "def mutate_wrapped: move (Wrapped, Wrapped) -> (Wrapped, Wrapped) = move pair => {\n",
                "  let mut copy = pair\n",
                "  copy[0] = Wrapped (c_string \"b\")\n",
                "  copy\n",
                "}\n",
                "def mutate_c_string: move (CString, CString) -> (CString, CString) = move pair => {\n",
                "  let mut copy = pair\n",
                "  copy[0] = c_string \"b\"\n",
                "  copy\n",
                "}\n",
                "def mutate_product: move ((I32, CString), (I32, CString)) -> ((I32, CString), (I32, CString)) = move pair => {\n",
                "  let mut copy = pair\n",
                "  copy[0] = (1, c_string \"b\")\n",
                "  copy\n",
                "}\n",
                "def mutate_nested: move (((I32, CString), I32), ((I32, CString), I32)) -> (((I32, CString), I32), ((I32, CString), I32)) = move pair => {\n",
                "  let mut copy = pair\n",
                "  copy[0] = ((1, c_string \"b\"), 2)\n",
                "  copy\n",
                "}\n",
                "let a = mutate_handle (Handle (c_string \"a\"), Handle (c_string \"c\"))\n",
                "let b = mutate_wrapped (Wrapped (c_string \"a\"), Wrapped (c_string \"c\"))\n",
                "let c = mutate_c_string (c_string \"a\", c_string \"c\")\n",
                "let d = mutate_product ((1, c_string \"a\"), (2, c_string \"c\"))\n",
                "let e = mutate_nested (((1, c_string \"a\"), 2), ((3, c_string \"c\"), 4))\n",
            ),
            concat!(
                "use std.cinterop.(CString, c_string)\n",
                "def pick: Bool -> (CString | I32) = condition => when { condition => c_string \"a\", else => 1 }\n",
                "def mutate_sum: move ((CString | I32), (CString | I32)) -> ((CString | I32), (CString | I32)) = move pair => {\n",
                "  let mut copy = pair\n",
                "  copy[0] = pick True\n",
                "  copy\n",
                "}\n",
                "let chosen = mutate_sum (pick False, pick True)\n",
            ),
            concat!(
                "use std.cinterop.(CString, c_string)\n",
                "type Box T = ctor (T)\n",
                "impl<T where Copy T> Drop (Box T) { def drop = Box value => () }\n",
                "def mutate_box: move (Box CString, Box CString) -> (Box CString, Box CString) = move pair => {\n",
                "  let mut copy = pair\n",
                "  copy[0] = Box (c_string \"b\")\n",
                "  copy\n",
                "}\n",
                "let boxed = mutate_box (Box (c_string \"a\"), Box (c_string \"c\"))\n",
            ),
        ] {
            let (found, stats) = assert_drop_glue_plans_agree(source);
            plans += found;
            max_rounds = max_rounds.max(stats.rounds);
            max_growth = max_growth.max(stats.growth);
        }
        assert!(
            plans >= 7,
            "the fixtures request several distinct drop-glue keys: {plans}"
        );
        assert!(
            max_rounds <= 4,
            "drop glue converges in a few rounds: {max_rounds} rounds, growth {max_growth}"
        );
        eprintln!(
            "stage 4.4 drop-glue transition fixtures: {plans} plans, max {max_rounds} rounds, max growth {max_growth}"
        );
    }

    // ------------------------------------------------------------------
    // Stage 4.3 formatting closure and legacy transition comparison.
    // ------------------------------------------------------------------

    /// Every string template in a materialized body binds its formatting
    /// helpers, every interpolation binds its selected callee, and every
    /// product-`Debug` plan binds `Formatter.write`.
    #[test]
    fn formatting_sites_bind_their_helpers_and_callees() {
        let lowered = lower(concat!(
            "def show_pair: (I32, I32) -> String = pair => \"${pair:?}\"\n",
            "def label: () -> String = () => \"plain\"\n",
            "def show_number: (I32) -> String = value => \"value=${value}\"\n",
            "let a = show_pair (1, 2)\n",
            "let b = label ()\n",
            "let c = show_number 1\n",
        ));
        let program = &lowered.program;
        let mut templates = 0;
        let mut interpolations = 0;
        for (_, instance) in program.instances.iter() {
            let Some(body) = &instance.body else {
                continue;
            };
            for (id, expression) in body.expressions.iter() {
                let LoweredExpressionKind::StringTemplate(template) = &expression.kind else {
                    continue;
                };
                templates += 1;
                assert!(
                    matches!(
                        body.binding(LoweredBindingSite::FormattingConstructor(id)),
                        Some(LoweredBoundTarget::Instance(_))
                    ),
                    "every template binds its constructor instance"
                );
                assert!(
                    matches!(
                        body.binding(LoweredBindingSite::FormattingFinish(id)),
                        Some(LoweredBoundTarget::Instance(_))
                    ),
                    "every template binds its finish instance"
                );
                let has_literal = template
                    .parts
                    .iter()
                    .any(|part| matches!(part, LoweredStringTemplatePart::Literal(_)));
                match body.binding(LoweredBindingSite::FormattingWrite(id)) {
                    Some(LoweredBoundTarget::Instance(_)) => {
                        assert!(
                            has_literal,
                            "write is bound only when a literal part exists"
                        );
                    }
                    None => {
                        assert!(
                            !has_literal,
                            "a template with a literal part binds the write instance"
                        );
                    }
                    other => panic!("unexpected formatting-write binding {other:?}"),
                }
                for (index, part) in template.parts.iter().enumerate() {
                    let LoweredStringTemplatePart::Interpolation(interpolation) = part else {
                        continue;
                    };
                    interpolations += 1;
                    let site = LoweredBindingSite::Interpolation {
                        template: id,
                        part: index,
                    };
                    match (&interpolation.evidence, body.binding(site)) {
                        (
                            TraitEvidence::ExplicitImplementation { .. },
                            Some(LoweredBoundTarget::Instance(_)),
                        ) => {}
                        (
                            TraitEvidence::Structural { .. },
                            Some(LoweredBoundTarget::Artifact(ordinal)),
                        ) => {
                            // A structural Debug reached through an
                            // interpolation must be reachable in the catalog.
                            let artifact = program
                                .artifacts
                                .get(LoweredArtifactRequestId::from_index(ordinal.index()))
                                .expect("the interpolation artifact is interned");
                            match &artifact.plan {
                                Some(LoweredArtifactPlan::StructuralMethod(plan)) => {
                                    if interpolation.format
                                        == staple_syntax::StringInterpolationFormat::Debug
                                    {
                                        assert!(
                                            matches!(
                                                plan.body,
                                                StructuralBody::ProductDebug { .. }
                                                    | StructuralBody::SumDebug { .. }
                                            ),
                                            "a Debug interpolation resolves a Debug body: {:?}",
                                            plan.body
                                        );
                                    }
                                }
                                other => panic!(
                                    "interpolation artifact has no structural plan: {other:?}"
                                ),
                            }
                        }
                        (evidence, target) => {
                            panic!("interpolation binding mismatch: {evidence:?} -> {target:?}")
                        }
                    }
                }
            }
        }
        assert!(
            templates >= 2,
            "the fixture covers a template with and without literals"
        );
        assert!(interpolations >= 2);

        let mut product_debug_plans = 0;
        for (_, artifact) in program.artifacts.iter() {
            if let Some(LoweredArtifactPlan::StructuralMethod(plan)) = &artifact.plan
                && let StructuralBody::ProductDebug { write, .. } = &plan.body
            {
                assert!(
                    write.instance.is_some(),
                    "every product-Debug plan binds the write instance"
                );
                product_debug_plans += 1;
            }
        }
        assert!(product_debug_plans > 0);
    }

    /// The completed arguments of a legacy structural body's key.
    fn canonical_arguments(arguments: &[crate::CheckedType]) -> Vec<CanonicalType> {
        let origin = Origin::compiler();
        arguments
            .iter()
            .map(|argument| {
                CanonicalType::concrete(argument, &origin).expect("concrete legacy argument")
            })
            .collect()
    }

    /// Requires every planned delegate to match the legacy delegate at the
    /// same position: the same instance template with the same concrete
    /// method type, or the same nested structural key.
    fn assert_delegates_match(
        program: &LoweredProgram,
        plan_delegates: &[(&PlannedCallee, &CheckedFunctionType)],
        legacy_delegates: &[crate::codegen::LegacyStructuralCallee],
    ) {
        use crate::codegen::LegacyStructuralCallee;
        let origin = Origin::compiler();
        assert_eq!(
            plan_delegates.len(),
            legacy_delegates.len(),
            "delegate count agrees"
        );
        for (entry, legacy) in plan_delegates.iter().zip(legacy_delegates) {
            let (callee, callee_type): (&PlannedCallee, &CheckedFunctionType) = *entry;
            let planned_type = CanonicalFunctionType::concrete(callee_type, &origin)
                .expect("concrete callee type");
            match (callee, legacy) {
                (
                    PlannedCallee::Instance(planned),
                    LegacyStructuralCallee::Instance(function, function_type),
                ) => {
                    let bound = planned.instance.expect("bound after closure");
                    let record = program.instances.get(bound).expect("bound instance");
                    assert_eq!(
                        record.template, *function,
                        "the delegated instance uses the legacy function template"
                    );
                    assert_eq!(
                        CanonicalFunctionType::concrete(function_type, &origin)
                            .expect("concrete legacy type"),
                        planned_type,
                        "the delegated method type agrees"
                    );
                    if let Some(body) = &record.body {
                        let signature = CanonicalFunctionType::concrete(&body.signature, &origin)
                            .expect("concrete body signature");
                        assert_eq!(signature, planned_type, "instance body signature agrees");
                    }
                }
                (
                    PlannedCallee::Artifact(planned),
                    LegacyStructuralCallee::Structural(structural, arguments),
                ) => {
                    let ordinal = planned.artifact.expect("bound after closure");
                    let key = program
                        .specializations
                        .artifact(ordinal)
                        .expect("bound artifact key");
                    match key {
                        ArtifactRequestKey::StructuralMethod(key) => {
                            assert_eq!(key.structural, *structural);
                            assert_eq!(key.arguments, canonical_arguments(arguments));
                        }
                        other => {
                            panic!("expected a structural key, got `{}`", other.family_name())
                        }
                    }
                }
                (planned, legacy) => {
                    panic!("delegate shape mismatch: planned {planned:?} vs legacy {legacy:?}")
                }
            }
        }
    }

    /// The observed coverage of one transition fixture.
    #[derive(Debug, Default)]
    struct TransitionCoverage {
        adapters: usize,
        methods: usize,
        kinds: std::collections::HashSet<StructuralTraitMethod>,
        value_adapters: usize,
        managed_ref_with_finalizer: usize,
        managed_ref_without_finalizer: usize,
    }

    /// Stage 4.3's transition comparison: every legacy-generated constructor
    /// adapter and structural body matches exactly one artifact plan with the
    /// same decisions, and every such plan matches a legacy body.
    fn assert_legacy_artifacts_match_plans(source: &str) -> TransitionCoverage {
        let mut coverage = TransitionCoverage::default();
        let module = checked_program(source);
        let lowered = Lowerer::new()
            .lower(&module)
            .expect("source should lower before the legacy comparison");
        let context = Context::create();
        let legacy = crate::codegen::legacy_emissions(&context, &lowered)
            .expect("the legacy backend should compile the module");
        let program = &lowered.program;
        let origin = Origin::compiler();

        for adapter in &legacy.constructor_adapters {
            let callable_type =
                CanonicalFunctionType::concrete(&adapter.callable_type, &origin).expect("concrete");
            let matches = program
                .artifacts
                .iter()
                .filter_map(|(_, artifact)| {
                    match (
                        program.specializations.artifact(artifact.ordinal),
                        artifact.plan.as_ref(),
                    ) {
                        (
                            Some(ArtifactRequestKey::ConstructorAdapter(key)),
                            Some(LoweredArtifactPlan::ConstructorAdapter(plan)),
                        ) if key.symbol == adapter.symbol && key.callable_type == callable_type => {
                            Some(plan)
                        }
                        _ => None,
                    }
                })
                .collect::<Vec<_>>();
            let [plan] = matches.as_slice() else {
                panic!(
                    "legacy constructor adapter for symbol {} matches {} plans, expected exactly one",
                    adapter.symbol.0,
                    matches.len()
                );
            };
            match &plan.construction {
                ConstructorConstruction::ManagedRef { finalizer, .. } => {
                    assert!(
                        adapter.managed_ref,
                        "legacy adapter for symbol {} returned a value but the plan allocates",
                        adapter.symbol.0
                    );
                    assert_eq!(
                        finalizer.is_some(),
                        adapter.finalizer_set,
                        "finalizer presence for symbol {}",
                        adapter.symbol.0
                    );
                    if finalizer.is_some() {
                        coverage.managed_ref_with_finalizer += 1;
                    } else {
                        coverage.managed_ref_without_finalizer += 1;
                    }
                }
                ConstructorConstruction::Value { .. } => {
                    assert!(
                        !adapter.managed_ref,
                        "legacy adapter for symbol {} allocates but the plan wraps a value",
                        adapter.symbol.0
                    );
                    assert!(!adapter.finalizer_set);
                    coverage.value_adapters += 1;
                }
                ConstructorConstruction::Unexpanded => {
                    panic!("constructor plan was never expanded")
                }
            }
            coverage.adapters += 1;
        }

        for method in &legacy.structural_methods {
            coverage.kinds.insert(method.structural);
            let arguments = canonical_arguments(&method.arguments);
            let function_type = CanonicalFunctionType::concrete(&method.function_type, &origin)
                .expect("concrete legacy method type");
            // Match by the legacy cache identity `(kind, arguments)` so a second
            // catalog plan that differs only in its callable type is caught as
            // a duplicate rather than silently skipped.
            let matches = program
                .artifacts
                .iter()
                .filter_map(|(_, artifact)| {
                    match (
                        program.specializations.artifact(artifact.ordinal),
                        artifact.plan.as_ref(),
                    ) {
                        (
                            Some(ArtifactRequestKey::StructuralMethod(key)),
                            Some(LoweredArtifactPlan::StructuralMethod(plan)),
                        ) if key.structural == method.structural && key.arguments == arguments => {
                            Some((key, plan))
                        }
                        _ => None,
                    }
                })
                .collect::<Vec<_>>();
            let [(key, plan)] = matches.as_slice() else {
                panic!(
                    "legacy {:?} body for {:?} matches {} plans, expected exactly one",
                    method.structural,
                    method.arguments,
                    matches.len()
                );
            };
            assert_eq!(
                key.callable_type, function_type,
                "legacy {:?} body for {:?} uses the plan's callable type",
                method.structural, method.arguments
            );
            match &plan.body {
                StructuralBody::ProductDebug { steps, write } => {
                    assert_eq!(method.structural, StructuralTraitMethod::Debug);
                    let literals = steps
                        .iter()
                        .filter_map(|step| match step {
                            DebugStep::Write(literal) => Some(literal.as_str()),
                            DebugStep::Element { .. } => None,
                        })
                        .collect::<Vec<_>>();
                    assert_eq!(
                        literals,
                        method
                            .debug_literals
                            .iter()
                            .map(String::as_str)
                            .collect::<Vec<_>>(),
                        "Debug literal sequence agrees"
                    );
                    let delegates = steps
                        .iter()
                        .filter_map(|step| match step {
                            DebugStep::Element { delegate, .. } => {
                                Some((&delegate.callee, &delegate.callee_type))
                            }
                            DebugStep::Write(_) => None,
                        })
                        .collect::<Vec<_>>();
                    assert_delegates_match(program, &delegates, &method.delegates);
                    assert!(write.instance.is_some(), "the write instance is bound");
                }
                StructuralBody::SumDebug { alternatives } => {
                    assert_eq!(method.structural, StructuralTraitMethod::Debug);
                    assert!(method.debug_literals.is_empty());
                    let delegates = alternatives
                        .iter()
                        .map(|delegate| (&delegate.callee, &delegate.callee_type))
                        .collect::<Vec<_>>();
                    assert_delegates_match(program, &delegates, &method.delegates);
                }
                StructuralBody::DerefIndexLoad { .. } => {
                    assert_eq!(method.structural, StructuralTraitMethod::DerefIndex);
                    assert_eq!(method.deref_index_fast_path, Some(true));
                    assert!(method.delegates.is_empty());
                }
                StructuralBody::DerefDelegate { delegate, .. } => {
                    match method.structural {
                        StructuralTraitMethod::DerefIndex => {
                            assert_eq!(method.deref_index_fast_path, Some(false));
                        }
                        StructuralTraitMethod::DerefMutateIndex => {
                            assert_eq!(method.deref_index_fast_path, None);
                        }
                        other => panic!("unexpected DerefDelegate for {other:?}"),
                    }
                    let delegates = vec![(&delegate.callee, &delegate.callee_type)];
                    assert_delegates_match(program, &delegates, &method.delegates);
                }
                StructuralBody::Next { done, yield_, .. } => {
                    assert_eq!(method.structural, StructuralTraitMethod::Iterator);
                    assert_eq!(method.next_alternatives, Some((done.index, yield_.index)));
                    assert!(method.delegates.is_empty());
                }
                StructuralBody::IndexSwitch { elements, output } => {
                    assert_eq!(method.structural, StructuralTraitMethod::Index);
                    assert_eq!(
                        method.index_homogeneous,
                        Some(false),
                        "the plan switches because legacy did"
                    );
                    assert_eq!(method.index_length, Some(elements.len()));
                    // The completed target argument is the source of truth for
                    // the element order and types.
                    let CheckedType::Product(target) = &method.arguments[0] else {
                        panic!("an Index target is a product");
                    };
                    assert_eq!(target.elements.len(), elements.len());
                    for (element, argument) in elements.iter().zip(&target.elements) {
                        assert_eq!(element.element, argument.value_type);
                        assert_eq!(
                            element.coercion.is_some(),
                            argument.value_type != *output,
                            "a coercion is recorded exactly when the types differ"
                        );
                        if let Some((from, to)) = &element.coercion {
                            assert_eq!(from, &argument.value_type);
                            assert_eq!(to, output);
                        }
                    }
                    assert!(method.delegates.is_empty(), "no delegated callees");
                    assert_eq!(method.deref_index_fast_path, None);
                    assert_eq!(method.next_alternatives, None);
                    assert!(method.debug_literals.is_empty());
                }
                StructuralBody::IndexLoad {
                    element,
                    length,
                    output,
                } => {
                    assert_eq!(method.structural, StructuralTraitMethod::Index);
                    assert_eq!(method.index_homogeneous, Some(true));
                    assert_eq!(method.index_length, Some(*length));
                    let CheckedType::Product(target) = &method.arguments[0] else {
                        panic!("an Index target is a product");
                    };
                    assert_eq!(target.homogeneous_element(), Some(element));
                    assert_eq!(target.elements.len(), *length);
                    assert_eq!(output, &method.arguments[2]);
                    assert!(method.delegates.is_empty(), "no delegated callees");
                    assert!(method.debug_literals.is_empty());
                }
                StructuralBody::MutateReplace {
                    element,
                    length,
                    drop_previous,
                } => {
                    assert_eq!(method.structural, StructuralTraitMethod::MutateIndex);
                    let CheckedType::Product(target) = &method.arguments[0] else {
                        panic!("a MutateIndex target is a product");
                    };
                    assert_eq!(target.elements.len(), *length);
                    assert_eq!(
                        method.mutate_drop_previous,
                        Some(drop_previous.is_some()),
                        "the planned drop-previous presence matches legacy"
                    );
                    if let Some(drop_previous) = drop_previous {
                        assert_eq!(
                            program
                                .specializations
                                .artifact(drop_previous.artifact.expect("bound after closure")),
                            Some(&drop_previous.key),
                        );
                    }
                    assert_eq!(element, &method.arguments[2]);
                    assert!(method.delegates.is_empty(), "no delegated callees");
                    assert_eq!(method.deref_index_fast_path, None);
                    assert!(method.debug_literals.is_empty());
                }
                StructuralBody::IntoIterator { source, iterator } => {
                    assert_eq!(method.structural, StructuralTraitMethod::IntoIterator);
                    assert_eq!(
                        method.into_iterator_source.as_ref(),
                        Some(source),
                        "the planned source matches the legacy source"
                    );
                    assert_eq!(&method.arguments[1], iterator);
                    assert!(method.delegates.is_empty(), "no delegated callees");
                    assert_eq!(method.deref_index_fast_path, None);
                    assert!(method.debug_literals.is_empty());
                }
                StructuralBody::Unexpanded => panic!("structural plan was never expanded"),
            }
            coverage.methods += 1;
        }

        // Vice versa: no catalog constructor or structural plan may exist
        // without an emitted legacy body.
        for (_, artifact) in program.artifacts.iter() {
            match program.specializations.artifact(artifact.ordinal) {
                Some(ArtifactRequestKey::ConstructorAdapter(key)) => {
                    assert!(
                        legacy.constructor_adapters.iter().any(|adapter| {
                            adapter.symbol == key.symbol
                                && CanonicalFunctionType::concrete(&adapter.callable_type, &origin)
                                    .expect("concrete")
                                    == key.callable_type
                        }),
                        "constructor plan for symbol {} has no legacy emission",
                        key.symbol.0
                    );
                }
                Some(ArtifactRequestKey::StructuralMethod(key)) => {
                    assert!(
                        legacy.structural_methods.iter().any(|method| {
                            method.structural == key.structural
                                && canonical_arguments(&method.arguments) == key.arguments
                                && CanonicalFunctionType::concrete(&method.function_type, &origin)
                                    .expect("concrete legacy method type")
                                    == key.callable_type
                        }),
                        "structural plan for {:?} has no legacy emission",
                        key.structural
                    );
                }
                _ => {}
            }
        }
        coverage
    }

    #[test]
    fn legacy_constructor_and_structural_bodies_match_artifact_plans() {
        let mut coverage = TransitionCoverage::default();
        for source in [
            concat!(
                "type Point = ctor (I32, I32)\n",
                "let make: () -> ((I32, I32) -> Point) = () => Point\n",
                "type Named = ctor (left: I32, right: I32)\n",
                "let make_named: () -> ((I32, I32) -> Named) = () => Named\n",
                "type Resource = ctor I32\n",
                "impl Drop Resource { def drop = Resource value => () }\n",
                "let make_resource: () -> (Resource -> Ref Resource) = () => Ref\n",
                "let make_ref: () -> (I32 -> Ref I32) = () => Ref\n",
                "def ref_maker: <T where Copy T> () -> (T -> Ref T) = () => Ref\n",
                "let maker_i32: I32 -> Ref I32 = ref_maker ()\n",
                "let maker_u8: U8 -> Ref U8 = ref_maker ()\n",
            ),
            concat!(
                "def show_pair: (I32, I32) -> String = pair => \"${pair:?}\"\n",
                "def show_named: (left: I32, right: I32) -> String = pair => \"${pair:?}\"\n",
                "def show_nested: ((I32, I32), (I32, I32)) -> String = nested => \"${nested:?}\"\n",
                "def pick: Bool -> (I32 | U8) = condition => when { condition => 1, else => (1 satisfies U8) }\n",
                "def show_sum: (I32 | U8) -> String = value => \"${value:?}\"\n",
                "let a = show_pair (1, 2)\n",
                "let b = show_named (1, 2)\n",
                "let c = show_nested ((1, 2), (3, 4))\n",
                "let d = show_sum (pick True)\n",
            ),
            concat!(
                "type Resource = ctor I32\n",
                "impl Drop Resource { def drop = Resource value => () }\n",
                "def index_mixed: (U8, I32) -> (I32 | U8) = pair => pair[0]\n",
                "def index_uniform: (I32, I32) -> I32 = pair => pair[0]\n",
                "def mutate_copy: (I32, I32) -> (I32, I32) = pair => { let mut copy = pair; copy[0] = 3; copy }\n",
                "def mutate_resource: move (Resource, Resource) -> (Resource, Resource) = move pair => {\n",
                "  let mut copy = pair\n",
                "  copy[0] = Resource 3\n",
                "  copy\n",
                "}\n",
                "def count_pair: (U8, I32) -> I32 = pair => {\n",
                "  let mut count = 0\n",
                "  for item in pair { count = count + 1 }\n",
                "  count\n",
                "}\n",
                "def deref_uniform: (Ref (I32, I32)) -> I32 = reference => reference[0]\n",
                "def deref_mixed: (Ref (U8, I32)) -> (I32 | U8) = reference => reference[0]\n",
                "def deref_replace: move (Ref (I32, I32)) -> Ref (I32, I32) = move reference => {\n",
                "  let mut own = reference\n",
                "  own[0] = 3\n",
                "  own\n",
                "}\n",
                "type Row = ctor (I32, I32)\n",
                "impl Index Row USize I32 { def index = (row, position) => 7 }\n",
                "def deref_row: (Ref Row, USize) -> I32 = (reference, position) => reference[position]\n",
                "def use_pair: <T where Copy T> (T, T) -> T = pair => {\n",
                "  let mut copy = pair\n",
                "  copy[0] = pair[1]\n",
                "  let mut last = copy[0]\n",
                "  for item in copy { last = item }\n",
                "  last\n",
                "}\n",
                "let used_i32: I32 = use_pair (1, 2)\n",
                "let used_u8: U8 = use_pair ((1 satisfies U8), (2 satisfies U8))\n",
                "let mixed = index_mixed ((1 satisfies U8), 2)\n",
                "let uniform = index_uniform (1, 2)\n",
                "let copied = mutate_copy (1, 2)\n",
                "let replaced = mutate_resource (Resource 1, Resource 2)\n",
                "let counted = count_pair ((1 satisfies U8), 2)\n",
                "let derefed = deref_uniform (Ref (1, 2))\n",
                "let derefed_mixed = deref_mixed (Ref ((1 satisfies U8), 2))\n",
                "let derefed_replaced = deref_replace (Ref (1, 2))\n",
                "let derefed_row = deref_row (Ref (Row (1, 2)), 0)\n",
            ),
            concat!(
                "type Held T = ctor (T)\n",
                "impl<T where Debug T> Debug (Held T) { def fmt = (Held value, mut formatter) => Debug.fmt (value, formatter) }\n",
                "def show_mixed: (Held I32, I32) -> String = pair => \"${pair:?}\"\n",
                "let text = show_mixed (Held 1, 2)\n",
            ),
        ] {
            let source_coverage = assert_legacy_artifacts_match_plans(source);
            coverage.adapters += source_coverage.adapters;
            coverage.methods += source_coverage.methods;
            coverage.kinds.extend(source_coverage.kinds);
            coverage.value_adapters += source_coverage.value_adapters;
            coverage.managed_ref_with_finalizer += source_coverage.managed_ref_with_finalizer;
            coverage.managed_ref_without_finalizer += source_coverage.managed_ref_without_finalizer;
        }
        assert!(
            coverage.adapters >= 5,
            "the fixtures cover multiple constructor adapter identities: {coverage:?}"
        );
        assert!(
            coverage.value_adapters > 0,
            "a wrapped-value adapter is compared: {coverage:?}"
        );
        assert!(
            coverage.managed_ref_with_finalizer > 0,
            "a managed-ref adapter with a finalizer is compared: {coverage:?}"
        );
        assert!(
            coverage.managed_ref_without_finalizer > 0,
            "a managed-ref adapter without a finalizer is compared: {coverage:?}"
        );
        for kind in [
            StructuralTraitMethod::Debug,
            StructuralTraitMethod::Index,
            StructuralTraitMethod::DerefIndex,
            StructuralTraitMethod::MutateIndex,
            StructuralTraitMethod::DerefMutateIndex,
            StructuralTraitMethod::IntoIterator,
            StructuralTraitMethod::Iterator,
        ] {
            assert!(
                coverage.kinds.contains(&kind),
                "the transition comparison covers {kind:?}: {coverage:?}"
            );
        }
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
        let signature = instances_of(program, template)[0]
            .1
            .body
            .as_ref()
            .expect("instance body")
            .signature
            .clone();
        assert!(
            program
                .instance_for_legacy_specialization(template, &signature, &substitutions)
                .is_none(),
            "a substitution with no instance must not match"
        );
    }

    #[test]
    fn legacy_specialization_requires_the_concrete_callable_type() {
        let lowered = lower(concat!(
            "def identity: <T where Copy T> T -> T = value => value\n",
            "let applied: I32 = identity 1\n",
        ));
        let program = &lowered.program;
        let template = function_id(program, "identity");
        let (_, instance) = single_instance(program, "identity");
        let parameter = instance.relevant.type_parameters().next().expect("T");
        let mut substitutions = std::collections::HashMap::new();
        substitutions.insert(parameter, crate::CheckedType::I32);
        let signature = &instance.body.as_ref().expect("instance body").signature;
        assert!(
            program
                .instance_for_legacy_specialization(template, signature, &substitutions)
                .is_some()
        );
        let mut wrong_signature = signature.clone();
        wrong_signature.result = Box::new(crate::CheckedType::U8);
        assert!(
            program
                .instance_for_legacy_specialization(template, &wrong_signature, &substitutions)
                .is_none()
        );
    }
}
