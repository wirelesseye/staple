//! Validation of the closed specialization graph.
//!
//! Checks catalog identity, instance bodies, artifact plans, dependency edges,
//! and recorded runtime requirements without reserving new keys or emitting IR.
//! Corruption tests exercise the invariants codegen relies on.

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
    /// The specialization graph audit: catalog/name agreement, complete concrete
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
        let names = match self
            .program
            .specializations
            .planned_names_with(self.program.declared_name_resolver())
        {
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
    /// evidence with the specialization canonical converters. A leftover declared
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
            TraitEvidence::DeclaredBound { .. } => self.report(
                origin,
                "emitted instance retains an unresolved trait evidence recipe",
            ),
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
pub(crate) mod tests {
    pub(crate) use crate::lower::census::*;
    use std::path::{Path, PathBuf};

    use inkwell::context::Context;

    use crate::specialization::{ArtifactRequestKey, GcFinalizerKey};
    use crate::{
        CallTypeSubstitution, CheckedType, LoweredArtifactPlan, LoweredModule, Lowerer,
        NameResolver, ProgramLoader, StructuralBody, SubstitutionEnvironment, TypeChecker,
        TypeParameterId, TypedModule,
    };

    use super::super::{
        LoweredArtifactDependencyKind, LoweredArtifactRequestId, LoweredArtifactRequestRoot,
        LoweredBindingSite, LoweredBoundTarget, LoweredCallStep, LoweredCallableCategory,
        LoweredCallableTarget, LoweredExpressionKind, LoweredInstanceDependency,
        LoweredInstanceDependencyKind, LoweredInstanceRequest, LoweredRepeatCount,
        LoweredStringTemplatePart, ProductionHooks, TraitEvidence,
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
                    parameter_product_capable: false,
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
            "impl TestShow I32 { test_show = _ => True }\n",
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
            parameter_product_capable: false,
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
            parameter_product_capable: false,
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
    // specialization fixtures over the full lowering pipeline.
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
            "type Phantom T = wrap ()\n",
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
            "type Point = wrap (I32, I32)\n",
            "let make: () -> ((I32, I32) -> Point) = () => Point\n",
            "def show_pair: (I32, I32) -> String = pair => \"${pair:?}\"\n",
            "def pick: Bool -> (I32 | U8) = condition => when { condition => 1, else => (1 satisfies U8) }\n",
            "def show_sum: (I32 | U8) -> String = value => \"${value:?}\"\n",
            "def index_mixed: (U8, I32) -> (I32 | U8) = pair => pair[0]\n",
            "def count_pair: (U8, I32) -> I32 = pair => {\n",
            "  let mut count = 0\n",
            "  for item in pair { count = count + 1 }\n",
            "  count\n",
            "}\n",
            "def deref_mixed: (Ref (U8, I32)) -> (I32 | U8) = reference => reference[0]\n",
            "let shown = show_pair (1, 2)\n",
            "let sum = show_sum (pick True)\n",
            "let picked = index_mixed ((1 satisfies U8), 2)\n",
            "let counted = count_pair ((1 satisfies U8), 2)\n",
            "let dereferenced = deref_mixed (Ref ((1 satisfies U8), 2))\n",
            "let p = (1, 2)\n",
            "let text = \"${p:?}\"\n",
        ));
        let program = &lowered.program;
        let mut saw_adapter = false;
        let mut saw_structural = false;
        for (_, artifact) in program.artifacts.iter() {
            match program.specializations.artifact(artifact.ordinal) {
                Some(ArtifactRequestKey::ConstructorAdapter(_)) => saw_adapter = true,
                // Structural artifact arguments are canonical keys, which can
                // only be built from concrete types.
                Some(ArtifactRequestKey::StructuralMethod(_)) => saw_structural = true,
                // Expansion records cleanup artifacts over the same catalog.
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

    /// Constructor-value and structural-method sites need no scanner: every
    /// such site in a materialized instance body is already bound to its
    /// artifact by the specialization worklist and binder, and every
    /// constructor or structural artifact is a specialization request root. No
    /// use-site variant is needed either.
    #[test]
    fn constructor_and_structural_sites_need_no_structural_scanner() {
        let lowered = lower(concat!(
            "type Point = wrap (I32, I32)\n",
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

        // Every constructor and structural artifact entered as a specialization
        // request root (from an initializer or an instance body), never as a
        // closure-phase discovery, so no structural scanner is needed.
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
                "artifact `{}` is not a specialization request root",
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
            "impl TestShow I32 { test_show = _ => True }\n",
            "trait TestGuarded T { guarded: T -> Bool }\n",
            "impl<T where TestShow T> TestGuarded T { guarded = value => test_show value }\n",
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
            "impl TestConvert I32 I32 I32 { test_convert = pair => pair.0 }\n",
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
        let root = std::env::temp_dir().join(format!("staple-cross-module-{}", std::process::id()));
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
    // Callback and coroutine cleanup regressions.
    // ------------------------------------------------------------------

    /// A reaction whose callback thunk captures a droppable value gets a
    /// closure-environment finalizer, and the reactive scanner requests the
    /// same plan at the reactive site.
    #[test]
    fn reactive_callback_environment_finalizer_is_planned() {
        let source = concat!(
            "use std.cinterop.(CString, c_string)\n",
            "extern \"c\" { inspect: CString -> I32 }\n",
            "def subscribe: move CString ->{Reactive} () = move captured => reaction { inspect captured; () }\n",
            "with Reactive = reactive_scope () { subscribe (c_string \"x\") }\n",
        );
        let module = checked_program(source);
        let lowered = Lowerer::new()
            .lower(&module)
            .unwrap_or_else(|diagnostics| panic!("source should lower: {diagnostics:?}"));
        let program = &lowered.program;

        // The fixture's reaction callback thunk captures a droppable value, so
        // lowering requests a finalizer for its environment.
        let reactive_thunks = program
            .reactive_callbacks
            .iter()
            .filter_map(|(_, callback)| callback.thunk)
            .collect::<Vec<_>>();
        assert!(
            !reactive_thunks.is_empty(),
            "the fixture records its reaction callback thunk"
        );
        // Every installed callback finalizer must have an expanded plan.
        let mut planned = 0;
        for thunk in reactive_thunks {
            let Some((instance, ordinal)) = program.instances.iter().find_map(|(id, instance)| {
                (instance.template == thunk).then_some((id, instance.ordinal))
            }) else {
                continue;
            };
            let Some(body) = program
                .instances
                .get(instance)
                .and_then(|i| i.body.as_ref())
            else {
                continue;
            };
            let captures = body.captures();
            let gate = captures.iter().any(|capture| {
                !capture.requires_initialization_state
                    && !capture.capture.borrowed
                    && program.concrete_needs_drop(&capture.value_type)
            });
            if !gate {
                continue;
            }
            let key = ArtifactRequestKey::GcFinalizer(GcFinalizerKey::ClosureEnvironment {
                closure: ordinal,
                captures: captures
                    .iter()
                    .map(|capture| canonical(&capture.value_type))
                    .collect(),
            });
            assert!(
                program.specializations.artifact_ordinal(&key).is_some(),
                "the installed callback environment finalizer has a plan: {key:?}"
            );
            planned += 1;
        }
        assert!(
            planned > 0,
            "the fixture's droppable callback capture installs a planned finalizer"
        );
    }

    /// Coroutine-body ownership agreement: `resume`
    /// emits with no `function_id`; the recorder now attributes state-0
    /// registrations to the body thunk, and the collector excludes frame
    /// cells. A completed coroutine never drops its droppable frame bindings:
    /// the emitter drops them only through the cancel unwind's conditional cell
    /// drop, which the pair plan carries as `unwind_drop`. The fixture asserts
    /// both the empty ownership records and the planned unwind drop, so the
    /// the emitter leak is mirrored and recorded rather than fixed.
    #[test]
    fn coroutine_frame_bindings_have_only_unwind_ownership() {
        let source = concat!(
            "use std.coroutine.*\n",
            "use std.cinterop.(CString, c_string)\n",
            "extern \"c\" { inspect: CString -> I32 }\n",
            "def task: () -> Coroutine{} I32 = () => coro {\n",
            "  let frame_value = c_string \"a\"\n",
            "  inspect frame_value\n",
            "}\n",
            "let created = task ()\n",
        );
        let module = checked_program(source);
        let lowered = Lowerer::new()
            .lower(&module)
            .unwrap_or_else(|diagnostics| panic!("source should lower: {diagnostics:?}"));
        let program = &lowered.program;

        // The fixture's coroutine body binds a droppable frame cell.
        let (body_instance, body) = program
            .instances
            .iter()
            .find_map(|(id, instance)| {
                let is_thunk = program
                    .functions
                    .get(instance.template)
                    .is_some_and(|function| function.class.coroutine_body);
                (is_thunk && instance.body.is_some())
                    .then_some((id, instance.body.as_ref().unwrap()))
            })
            .expect("the fixture instantiates its coroutine body thunk");
        assert!(
            body.plan_template.is_some(),
            "the body instance owns its coroutine plan"
        );

        assert!(
            body.owned_bindings.is_empty(),
            "a coroutine body owns no scope-exit bindings: every local is a frame cell"
        );

        // The completed-body frame-binding leak, mirrored: the pair plan still
        // carries the cancel unwind's conditional cell drop for the frame
        // binding, and the emitter emits it through the unwind path, but no
        // completion path drops it.
        let plan_id = body.plan_template.expect("the body owns its plan");
        let plan = body.plan(plan_id).expect("the local plan");
        let frame_binding = plan
            .frame_bindings
            .iter()
            .find(|symbol| {
                body.binding_symbol_type(**symbol)
                    .is_some_and(|value_type| *value_type == CheckedType::CString)
            })
            .copied()
            .expect("the fixture's CString frame binding");
        let pair = program
            .artifacts
            .iter()
            .filter_map(|(_, artifact)| match artifact.plan.as_ref() {
                Some(crate::LoweredArtifactPlan::CoroutineCodes(plan))
                    if plan.body.index() == body_instance.index() =>
                {
                    plan.frame.as_ref()
                }
                _ => None,
            })
            .next()
            .expect("the body thunk's expanded pair");
        let unwind = pair
            .frame_bindings
            .iter()
            .find(|binding| binding.symbol == frame_binding)
            .and_then(|binding| binding.unwind_drop.as_ref())
            .expect("the frame binding plans its unwind drop");
        assert!(unwind.artifact.is_some(), "the unwind drop glue is bound");
    }

    // ------------------------------------------------------------------
    // Extern adapters and runtime requirements.
    // ------------------------------------------------------------------

    const EXTERN_ADAPTER_FIXTURE: &str = concat!(
        "use std.cinterop.(CString, c_string)\n",
        "extern \"c\" { inspect: CString -> I32 }\n",
        "extern \"c\" { unused_extern: CString -> I32 }\n",
        "def apply: ((CString -> I32), CString) -> I32 = (f, value) => f value\n",
        "def forward: ((CString -> I32) -> (CString -> I32)) = f => f\n",
        "let direct = inspect (c_string \"d\")\n",
        "let indirect = apply (inspect, c_string \"i\")\n",
        "let forwarded = forward inspect\n",
        "let result = forwarded (c_string \"f\")\n",
    );

    /// How many closure-phase uses of one artifact are `ExternAdapterValue`
    /// sites.
    fn extern_adapter_use_count(
        program: &LoweredProgram,
        artifact: crate::specialization::ArtifactOrdinal,
    ) -> usize {
        let counts = |uses: &[crate::LoweredArtifactUse]| {
            uses.iter()
                .filter(|use_| {
                    use_.artifact == artifact
                        && use_.kind == LoweredArtifactDependencyKind::ExternAdapter
                        && matches!(use_.site, crate::ArtifactUseSite::ExternAdapterValue(_))
                })
                .count()
        };
        let mut total = 0;
        for (_, instance) in program.instances.iter() {
            if let Some(body) = &instance.body {
                total += counts(&body.artifact_uses);
            }
        }
        for uses in &program.initializer_artifact_uses {
            total += counts(uses);
        }
        total
    }

    #[test]
    fn extern_adapter_plans_cover_used_values_and_deduplicate_sites() {
        let module = checked_program(EXTERN_ADAPTER_FIXTURE);
        let lowered = Lowerer::new()
            .lower(&module)
            .expect("the extern fixture lowers and validates");
        let program = &lowered.program;

        // The owned catalog has one expanded artifact per extern adapter that
        // a callable-value site reaches, and every use is bound to it.
        let mut artifacts = Vec::new();
        for (id, artifact) in program.artifacts.iter() {
            let Some(crate::LoweredArtifactPlan::ExternAdapter(plan)) = &artifact.plan else {
                continue;
            };
            let Some(ArtifactRequestKey::ExternAdapter(key)) =
                program.specializations.artifact(artifact.ordinal)
            else {
                panic!("extern adapter artifact {id:?} has a mismatched key");
            };
            let declaration = plan
                .declaration
                .unwrap_or_else(|| panic!("extern adapter artifact {id:?} is expanded"));
            assert!(
                declaration.eagerly_declared,
                "the plan records the emitter eager foreign-symbol declaration: {plan:?}"
            );
            let expected_arity = if plan.callable_type.parameter_style
                == staple_syntax::FunctionParameterStyle::Juxtaposed
            {
                match plan.callable_type.parameter.as_ref() {
                    crate::CheckedType::Product(product) => product.elements.len(),
                    _ => 1,
                }
            } else {
                1
            };
            assert_eq!(
                declaration.arity, expected_arity,
                "the recorded arity matches the adapter's parameter shape"
            );
            let uses = extern_adapter_use_count(program, artifact.ordinal);
            assert!(
                uses >= 1,
                "extern adapter artifact {id:?} has at least one callable-value use"
            );
            artifacts.push((key.symbol, key.callable_type.clone(), uses));
        }
        assert!(
            !artifacts.is_empty(),
            "the fixture uses extern bindings as first-class values"
        );

        let unused = program
            .symbols
            .iter()
            .find_map(|(_, id, symbol)| (symbol.name == "unused_extern").then_some(id))
            .expect("the unused extern is in the symbol catalog");
        assert!(
            artifacts.iter().all(|(symbol, _, _)| *symbol != unused),
            "an unused extern has no callable adapter artifact"
        );
        assert!(
            artifacts.iter().any(|(_, _, uses)| *uses > 1),
            "two callable-value sites dedup to one adapter artifact: {artifacts:?}"
        );
    }

    #[test]
    fn variadic_extern_values_are_rejected_at_lowering() {
        let module = checked_program(concat!(
            "extern \"c\" { report: (I32, ...) -> I32 }\n",
            "def forward: ((I32, ...) -> I32) -> ((I32, ...) -> I32) = f => f\n",
            "let forwarded = forward report\n",
        ));
        let diagnostics = Lowerer::new()
            .lower(&module)
            .expect_err("a variadic extern used as a value is rejected");
        assert!(
            diagnostics.iter().any(|diagnostic| diagnostic
                .message
                .contains("variadic external functions cannot be used as first-class values")),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn corrupted_extern_adapter_plans_and_uses_are_diagnosed() {
        let module = checked_program(EXTERN_ADAPTER_FIXTURE);
        let mut lowered = Lowerer::new()
            .lower(&module)
            .expect("the extern fixture lowers and validates");

        // A plan that lost its expansion marker is rejected.
        let mut broken = lowered.clone();
        let artifact = broken
            .program
            .artifacts
            .iter()
            .find(|(_, artifact)| {
                matches!(
                    artifact.plan,
                    Some(crate::LoweredArtifactPlan::ExternAdapter(_))
                )
            })
            .map(|(id, _)| id)
            .expect("an extern adapter artifact");
        if let Some(crate::LoweredArtifactPlan::ExternAdapter(plan)) = broken
            .program
            .artifacts
            .get_mut(artifact)
            .and_then(|artifact| artifact.plan.as_mut())
        {
            plan.declaration = None;
        }
        let diagnostics = broken.program.validate_artifact_closure(&ProductionHooks);
        assert!(
            diagnostics.iter().any(|diagnostic| diagnostic
                .message
                .contains("still carries the request-time plan marker")),
            "{diagnostics:?}"
        );

        // A use whose callable value no longer names the adapter's symbol.
        // The fixture's callable values live in module initializers, so the
        // program's template arena owns them.
        let (value_id, site_origin) = lowered
            .program
            .initializer_artifact_uses
            .iter()
            .flat_map(|uses| uses.iter())
            .find_map(|use_| {
                let crate::ArtifactUseSite::ExternAdapterValue(value) = use_.site else {
                    return None;
                };
                Some((value, use_.origin.clone()))
            })
            .expect("an extern adapter use site");
        if let Some(value) = lowered.program.callable_values.get_mut(value_id) {
            value.target = LoweredCallableTarget::ExternalFunction {
                symbol: crate::SymbolId(9_999),
            };
        }
        let diagnostics = lowered.program.validate_artifact_closure(&ProductionHooks);
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.span == site_origin.span
                    && diagnostic.message.contains(
                        "an extern adapter use names a different symbol or callable type"
                    )),
            "{diagnostics:?}"
        );
    }

    /// The recorded requirements of one fixture, in canonical order.
    fn runtime_requirements(source: &str) -> Vec<crate::RuntimeRequirement> {
        lower(source)
            .program
            .runtime_requirements
            .requirements()
            .to_vec()
    }

    #[test]
    fn unused_subsystems_record_no_requirement() {
        use crate::RuntimeRequirement::*;

        // The backend emits every eagerly declared standard-library template,
        // so the surfaces those bodies reference are required by every program
        // that links the library. The test therefore checks the two surfaces
        // no eager body references, and that adding a use adds its surface.
        let base = runtime_requirements("let answer: I32 = 1 + 2\n");
        assert!(
            base.contains(&GarbageCollector)
                && base.contains(&ReactiveRuntime)
                && base.contains(&Utf8Validator)
                && base.contains(&CStringFree)
                && base.contains(&NumericToString)
                && base.contains(&CStringLength)
                && base.contains(&InteriorNulCheck),
            "the eagerly emitted library bodies reference these surfaces: {base:?}"
        );
        assert!(
            !base.contains(&CoroutineRuntime),
            "a program without coroutines records no coroutine runtime: {base:?}"
        );
        assert!(
            !base.contains(&LiteralComparison),
            "a program without string patterns records no literal comparison: {base:?}"
        );

        let pattern = runtime_requirements(concat!(
            "def classify: String -> I32 = value => match value { \"a\" => 1, _ => 0 }\n",
            "let result = classify \"a\"\n",
        ));
        assert!(
            pattern.contains(&LiteralComparison),
            "string-literal matching adds `memcmp`: {pattern:?}"
        );

        let coroutine = runtime_requirements(concat!(
            "use std.coroutine.*\n",
            "def task: () -> Coroutine{} I32 = () => coro { 1 }\n",
            "let handle = task ()\n",
        ));
        assert!(
            coroutine.contains(&CoroutineRuntime),
            "a coroutine frame needs the coroutine runtime: {coroutine:?}"
        );
        assert!(
            coroutine.contains(&GarbageCollector),
            "a coroutine frame allocates through the collector: {coroutine:?}"
        );
    }

    /// Independent LLVM uses for every emitted instance and initializer owner,
    /// keyed by planned names (including multiple generic instances).
    fn assert_owner_requirements_match_emitted_functions(
        program: &LoweredProgram,
        emitted: &crate::codegen::LoweredEmissions,
    ) -> usize {
        use super::super::emission::OwnerArenas;
        use crate::RuntimeRequirement;
        use std::collections::HashSet;
        let mut compared = 0;
        let mut owners = Vec::new();
        for (id, instance) in program.instances.iter() {
            let Some(body) = &instance.body else {
                continue;
            };
            let name = program.planned_name(id).expect("planned instance name");
            owners.push((
                name,
                OwnerArenas::Instance(body),
                body.artifact_uses.as_slice(),
            ));
        }
        for (id, initializer) in program.initializers.iter() {
            let uses = program
                .initializer_artifact_uses
                .get(id.index())
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            owners.push((
                initializer.name.as_str(),
                OwnerArenas::Initializer(id),
                uses,
            ));
        }
        for (name, owner, uses) in owners {
            if !emitted.defined_functions.contains(name) {
                continue;
            }
            let derived = program
                .owner_runtime_requirements(owner, uses)
                .requirements()
                .iter()
                .copied()
                .collect::<HashSet<_>>();
            let referenced = emitted
                .runtime_references
                .iter()
                .filter(|(_, function)| function == name)
                .filter_map(|(symbol, _)| RuntimeRequirement::for_runtime_symbol(symbol))
                .collect::<HashSet<_>>();
            // A plan can conservatively require an installed subsystem without
            // calling it from this owner (plain coroutine creation is inline).
            assert!(
                referenced.is_subset(&derived),
                "`{name}` references unrecorded runtime surfaces: {:?}",
                referenced.difference(&derived).collect::<Vec<_>>()
            );
            compared += 1;
        }
        compared
    }

    #[test]
    fn runtime_requirements_cover_lowered_surfaces() {
        use crate::RuntimeRequirement;
        use std::collections::HashSet;

        let fixtures = [
            concat!(
                "use std.cinterop.(CString, c_string)\n",
                "extern \"c\" { inspect: CString -> I32 }\n",
                "def convert: move CString -> String = move value => CString.to_string value\n",
                "def render: String -> CString = value => CString.from_string value\n",
                "def classify: String -> I32 = value => match value { \"a\" => 1, _ => 0 }\n",
                "def owned: () -> CString = () => c_string \"owned\"\n",
                "let text: String = to_string 1\n",
                "let a = convert (c_string \"x\")\n",
                "let b = render \"text\"\n",
                "let c = classify \"a\"\n",
                "let d = owned ()\n",
                "let dropped = inspect (c_string \"z\")\n",
                "let joined = a + \"y\"\n",
                "let rendered = \"rank=$c\"\n",
            ),
            concat!(
                "use std.coroutine.*\n",
                "use std.cinterop.(CString, c_string)\n",
                "extern \"c\" { inspect: CString -> I32 }\n",
                "let signal flag = 0\n",
                "def task: () -> Coroutine{} I32 = () => coro { 1 }\n",
                "def waiting: () -> Coroutine{Reactive} () = () => coro {\n",
                "  let _ = await (until { flag >= 1 })\n",
                "  ()\n",
                "}\n",
                "def run: () -> Coroutine{} I32 = () => coro {\n",
                "  let first = await (coro { 2 })\n",
                "  first\n",
                "}\n",
                "def owning: move CString -> Coroutine{} I32 = move value => coro { inspect value; 1 }\n",
                "let handle = task ()\n",
                "let driven = run ()\n",
                "let subscribed = with Reactive = reactive_scope () { reaction { () } }\n",
                "let derived = flag + flag\n",
                "let owned = owning (c_string \"x\")\n",
            ),
            concat!(
                "use std.buffer.*\n",
                "use std.clone.Clone\n",
                "let mut values: Buffer I32 = Buffer.with_capacity (4 satisfies USize)\n",
                "Buffer.push values 1\n",
                "let copied = Clone.clone values\n",
            ),
            concat!(
                "use std.coroutine.*\n",
                "use std.cinterop.(CString, c_string)\n",
                "extern \"c\" { inspect: CString -> I32 }\n",
                "let signal count = 0\n",
                "def waiter: move Wait I32 -> Coroutine{} I32 = move pending => coro {\n",
                "  let _ = await pending\n",
                "  0\n",
                "}\n",
                "def make_completion: () -> (wait: Wait I32, resolver: Resolver I32) = () => completion (scheduler ())\n",
                "def drive_wait: () -> Coroutine{} I32 = () => {\n",
                "  let (wait, resolver) = make_completion ()\n",
                "  let _ = resolver\n",
                "  waiter wait\n",
                "}\n",
                "def driver: () -> Coroutine{} I32 = () => coro { let v = await (coro { 7 }); v + 1 }\n",
                "let a = drive_wait ()\n",
                "let sched = scheduler ()\n",
                "with Tasks = task_scope (sched) {\n",
                "  let _ = spawn (driver ())\n",
                "  let _ = pump (sched, 4)\n",
                "}\n",
            ),
        ];

        let mut covered = HashSet::new();
        let mut compared = 0;
        for source in fixtures {
            let module = checked_program(source);
            let lowered = Lowerer::new().lower(&module).unwrap_or_else(|diagnostics| {
                panic!("fixture should lower: {diagnostics:?}\n{source}")
            });
            let context = Context::create();
            let emitted = crate::codegen::lowered_emissions(&context, &lowered).unwrap_or_else(
                |diagnostics| panic!("fixture should compile: {diagnostics:?}\n{source}"),
            );
            let requirements = lowered
                .program
                .runtime_requirements
                .requirements()
                .iter()
                .copied()
                .collect::<HashSet<_>>();
            let mut mapped = HashSet::new();
            for (name, _) in &emitted.runtime_references {
                if let Some(requirement) = RuntimeRequirement::for_runtime_symbol(name) {
                    mapped.insert(requirement);
                }
            }
            assert_eq!(
                requirements, mapped,
                "the recorded requirements must match the surfaces emitted emission references\n{source}"
            );
            covered.extend(requirements);
            compared +=
                assert_owner_requirements_match_emitted_functions(&lowered.program, &emitted);
        }
        assert!(
            compared >= 10,
            "the per-function comparison covers many emitted functions: {compared}"
        );
        for requirement in RuntimeRequirement::ALL {
            assert!(
                covered.contains(&requirement),
                "the fixtures cover `{}`",
                requirement.description()
            );
        }
    }

    #[test]
    fn owner_requirements_match_each_emitted_function() {
        use crate::RuntimeRequirement;
        use std::collections::HashSet;

        // The program-wide set is masked by the eagerly emitted standard
        // library, which already needs the collector. A per-function
        // comparison is not: each body's own derivation must equal the
        // surfaces its emitted function references. `second` is read by
        // `first` before its initializer, so emitted predeclares it with a
        // malloc'd state cell registered as a GC root region. `first`'s
        // environment, which captures `second`, is GC-allocated in the same
        // function, so the collector is required either way; the check is
        // that the per-function sets agree, in a body and in a loop body.
        let source = concat!(
            "def forward: () -> I32 = () => {\n",
            "  def first = () => second ()\n",
            "  def second = () => 42\n",
            "  first ()\n",
            "}\n",
            "def looped: () -> I32 = () => {\n",
            "  loop {\n",
            "    def first = () => second ()\n",
            "    def second = () => 7\n",
            "    break first ()\n",
            "  }\n",
            "}\n",
            "let a = forward ()\n",
            "let b = looped ()\n",
        );
        let module = checked_program(source);
        let lowered = Lowerer::new()
            .lower(&module)
            .unwrap_or_else(|diagnostics| panic!("fixture should lower: {diagnostics:?}"));
        let context = Context::create();
        let emitted = crate::codegen::lowered_emissions(&context, &lowered)
            .unwrap_or_else(|diagnostics| panic!("fixture should compile: {diagnostics:?}"));
        let program = &lowered.program;

        for name in ["forward", "looped"] {
            let body = program
                .instances
                .iter()
                .find(|(_, instance)| {
                    program
                        .functions
                        .get(instance.template)
                        .is_some_and(|function| function.name == name)
                })
                .and_then(|(_, instance)| instance.body.as_ref())
                .unwrap_or_else(|| panic!("`{name}` has a materialized instance"));
            let derived = program
                .owner_runtime_requirements(
                    super::super::emission::OwnerArenas::Instance(body),
                    &body.artifact_uses,
                )
                .requirements()
                .iter()
                .copied()
                .collect::<HashSet<_>>();
            let emitted = emitted
                .runtime_references
                .iter()
                .filter(|(_, function)| function == name)
                .filter_map(|(symbol, _)| RuntimeRequirement::for_runtime_symbol(symbol))
                .collect::<HashSet<_>>();
            assert!(
                emitted.contains(&RuntimeRequirement::GarbageCollector),
                "emitted registers `{name}`'s predeclared state cell as a GC root: {:?}",
                emitted
            );
            assert_eq!(
                derived, emitted,
                "`{name}`'s derived requirements match its emitted function's references"
            );
        }
    }

    #[test]
    fn runtime_requirements_are_deterministic_and_validated() {
        use crate::RuntimeRequirement;

        let source = concat!(
            "use std.cinterop.(CString, c_string)\n",
            "let text: String = CString.to_string (c_string \"x\")\n",
            "let joined: String = text + \"y\"\n",
        );
        let first = lower(source);
        let second = lower(source);
        assert_eq!(
            first.program.runtime_requirements, second.program.runtime_requirements,
            "repeated lowering records the same requirements"
        );

        // A cleared set is diagnosed as missing.
        let mut broken = lower(source);
        broken.program.runtime_requirements = crate::LoweredRuntimeRequirements::default();
        let diagnostics = broken.program.validate_artifact_closure(&ProductionHooks);
        assert!(
            diagnostics.iter().any(|diagnostic| diagnostic
                .message
                .contains("is missing from the recorded set")),
            "{diagnostics:?}"
        );

        // A requirement no lowered operation needs is diagnosed as extra.
        let mut broken = lower("let answer: I32 = 1 + 2\n");
        broken
            .program
            .runtime_requirements
            .record(RuntimeRequirement::CoroutineRuntime);
        let diagnostics = broken.program.validate_artifact_closure(&ProductionHooks);
        assert!(
            diagnostics.iter().any(|diagnostic| diagnostic
                .message
                .contains("has no lowered operation that needs it")),
            "{diagnostics:?}"
        );
    }

    // ------------------------------------------------------------------
    // Drop-glue agreement tests.
    // ------------------------------------------------------------------

    /// Every naturally requested `DropGlue` plan agrees with the typed module:
    /// the key's type needs drop, a user-drop body is planned exactly when the
    /// general drop-implementation predicate applies, and the
    /// checker and lowering `needs_drop`/`Copy` predicates agree on every
    /// concrete type the fixture's catalog reaches.
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
        let mut checked_types = Vec::new();
        for (_, artifact) in program.artifacts.iter() {
            let Some(crate::LoweredArtifactPlan::DropGlue(plan)) = &artifact.plan else {
                continue;
            };
            plans += 1;
            checked_types.push(plan.value_type.clone());
            assert!(
                module.type_needs_drop(&plan.value_type),
                "drop glue `{}` is only requested for a droppable type",
                plan.value_type
            );
            assert_eq!(
                module.type_needs_drop(&plan.value_type),
                program.concrete_needs_drop(&plan.value_type),
                "needs-drop diverges for `{}`",
                plan.value_type
            );
            assert_eq!(
                module.is_copy_type(&plan.value_type),
                program.concrete_is_copy(&plan.value_type),
                "Copy diverges for `{}`",
                plan.value_type
            );
            let applies = program.concrete_drop_implementation_applies(&plan.value_type);
            match &plan.body {
                crate::DropGlueBody::Unexpanded => {
                    panic!("drop glue `{}` was never expanded", plan.value_type)
                }
                crate::DropGlueBody::UserDrop {
                    method,
                    representation: _,
                } => {
                    assert!(
                        applies,
                        "a planned user drop for `{}` has no matching implementation",
                        plan.value_type
                    );
                    assert!(
                        method.instance.is_some(),
                        "the user drop for `{}` is bound after closure",
                        plan.value_type
                    );
                    assert_eq!(
                        method.kind,
                        LoweredInstanceDependencyKind::DropMethod,
                        "the user-drop edge uses the drop-method kind"
                    );
                }
                _ => assert!(
                    !applies,
                    "a non-user body is planned for `{}` although a user implementation matches",
                    plan.value_type
                ),
            }
        }
        for value_type in checked_types {
            assert_eq!(
                module.type_needs_drop(&value_type),
                program.concrete_needs_drop(&value_type),
                "needs-drop diverges for `{value_type}`"
            );
            assert_eq!(
                module.is_copy_type(&value_type),
                program.concrete_is_copy(&value_type),
                "Copy diverges for `{value_type}`"
            );
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
                "type Resource = wrap I32\n",
                "impl Drop Resource { drop = Resource value => () }\n",
                "def mutate_resource: move (Resource, Resource) -> (Resource, Resource) = move pair => {\n",
                "  let mut copy = pair\n",
                "  copy[0] = Resource 3\n",
                "  copy\n",
                "}\n",
                "let replaced = mutate_resource (Resource 1, Resource 2)\n",
            ),
            concat!(
                "use std.cinterop.(CString, c_string)\n",
                "type Handle = wrap CString\n",
                "impl Drop Handle { drop = Handle value => () }\n",
                "type Wrapped = wrap CString\n",
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
                "type Box T = wrap (T)\n",
                "impl<T where Copy T> Drop (Box T) { drop = Box value => () }\n",
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
            "artifact drop-glue cleanup fixtures: {plans} plans, max {max_rounds} rounds, max growth {max_growth}"
        );
    }

    // ------------------------------------------------------------------
    // Formatting closure and runtime requirement tests.
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

    /// Non-generic declarations keep their available names verbatim;
    /// reserved, duplicate and generic instances use catalog ordinal names.
    #[test]
    fn planned_names_preserve_unreserved_declarations() {
        let source = concat!(
            "use std.cinterop.(CString, c_string)\n",
            "def plain: I32 -> I32 = value => value\n",
            "def generic: <T where Copy T> T -> T = value => value\n",
            "let first = plain (1)\n",
            "let second = generic 2\n",
            "let owned = c_string \"x\"\n",
        );
        let lowered = lower(source);
        let program = &lowered.program;
        let reserved = program.reserved_symbol_names();
        let mut taken = std::collections::HashSet::new();
        let mut compared = Vec::new();
        let mut fallbacks = Vec::new();
        for (id, instance) in program.instances.iter() {
            let key = program
                .specializations
                .instance(instance.ordinal)
                .expect("instance key");
            let declared = &program
                .functions
                .get(instance.template)
                .expect("template")
                .name;
            let planned = program.planned_name(id).expect("planned name");
            if key.substitutions().is_empty() && key.evidence().is_none() {
                if !reserved.contains(declared) && !taken.contains(declared) {
                    assert_eq!(planned, declared, "an available declaration keeps its name");
                } else {
                    assert_eq!(
                        planned,
                        format!("__staple_instance_{}", instance.ordinal.index())
                    );
                    fallbacks.push(declared.clone());
                }
                compared.push(planned.to_owned());
            }
            taken.insert(planned.to_owned());
        }
        for (shape, found) in [
            (
                "a single-module user function keeps its bare name",
                compared.iter().any(|name| name == "plain"),
            ),
            (
                "a standard-library function keeps its mangled name",
                compared.iter().any(|name| {
                    name.starts_with("__staple_mstd.") && !name[3..].contains("__staple_m")
                }),
            ),
            (
                "no planned name is double-prefixed",
                compared
                    .iter()
                    .all(|name| name.matches("__staple_m").count() <= 1),
            ),
        ] {
            assert!(found, "{shape}: {compared:?}");
        }
        assert!(
            fallbacks.iter().any(|name| name == "write"),
            "the standard-library `write` method clashes with the reactive runtime's libc `write`: {fallbacks:?}"
        );
        let generic = function_id(program, "generic");
        let (generic_instance, _) = program
            .instances
            .iter()
            .find(|(_, instance)| instance.template == generic)
            .expect("generic has an instance");
        assert_eq!(
            program.planned_name(generic_instance),
            Some(format!("__staple_instance_{}", generic_instance.index()).as_str()),
            "a generic instance keeps its ordinal name"
        );
    }

    /// An indexed (`MutateIndex`) assignment never records a
    /// replaced-value drop (the method replaces the element; the emitter drops
    /// nothing at the site), in instance bodies as in lowering, and a
    /// droppable materialized base records `MutateIndexTemporary`.
    #[test]
    fn indexed_assignment_cleanup_facts_match_checked_rules() {
        let lowered = lower(concat!(
            "type Holder = wrap I32\n",
            "impl Drop Holder { drop = Holder value => () }\n",
            "type Item = wrap I32\n",
            "impl Drop Item { drop = Item value => () }\n",
            "impl Index Holder I32 Item { index = (holder, key) => Item 0 }\n",
            "impl MutateIndex Holder I32 Item { mutate_index = (mut holder, key, move value) => () }\n",
            "def make_holder: () -> Holder = () => Holder 0\n",
            "def assign_temp: () -> () = () => { (make_holder ())[0] = Item 1 }\n",
            "def assign_place: () -> () = () => {\n",
            "  let mut holder = make_holder ()\n",
            "  holder[0] = Item 2\n",
            "}\n",
            "let at = assign_temp ()\n",
            "let ap = assign_place ()\n",
        ));
        let program = &lowered.program;
        let body_of = |name: &str| {
            let template = function_id(program, name);
            program
                .instances
                .iter()
                .find(|(_, instance)| instance.template == template)
                .and_then(|(_, instance)| instance.body.as_ref())
                .unwrap_or_else(|| panic!("`{name}` has a materialized body"))
        };
        for (name, base_temporary) in [("assign_temp", true), ("assign_place", false)] {
            let body = body_of(name);
            let assignments = body
                .items
                .iter()
                .filter_map(|(id, item)| match &item.kind {
                    LoweredItemKind::Assignment(assignment)
                        if assignment.mutate_index.is_some() =>
                    {
                        Some((id, assignment))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(assignments.len(), 1, "`{name}` has one indexed assignment");
            let (item, assignment) = assignments[0];
            assert!(
                !assignment.drop_previous,
                "`{name}`: an indexed assignment never drops the previous value"
            );
            assert_eq!(assignment.drops_base_temporary, base_temporary, "`{name}`");
            assert!(
                !body
                    .artifact_uses
                    .iter()
                    .any(|use_| use_.site == crate::ArtifactUseSite::ReplacedValue(item)),
                "`{name}`: no replaced-value drop is recorded for an indexed assignment"
            );
            assert_eq!(
                body.artifact_uses
                    .iter()
                    .any(|use_| use_.site == crate::ArtifactUseSite::MutateIndexTemporary(item)),
                base_temporary,
                "`{name}`: the base-temporary drop is recorded exactly when the base is a droppable temporary"
            );
        }
    }
}
