//! Validation of closed generated-artifact plans.
//!
//! Catalog closure checks key identity, concrete callees, dependency edges, and
//! use agreement. This module checks body completeness and recursively recorded
//! type facts before a lowered program can cross the emission boundary.

use staple_syntax::Diagnostic;

use super::{ArenaId, LoweredProgram, PlanType};
use crate::specialization::{CanonicalFunctionType, CanonicalType};

impl LoweredProgram {
    /// The closed-catalog checks artifact planning adds on top of the artifact planning
    /// validators. Runs after `validate_artifact_closure`.
    pub(super) fn validate_closed_catalog(&self) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        self.check_plan_types(&mut diagnostics);
        for (id, expected) in self.planned_initializer_names() {
            let initializer = self.initializers.get(id).expect("initializer exists");
            if initializer.name != expected {
                diagnostics.push(Diagnostic::new(
                    initializer.origin.span.clone(),
                    "initializer name disagrees with the collision-free plan",
                ));
            }
        }
        diagnostics
    }

    /// Every type in every plan canonicalizes concretely: no declared type
    /// parameter, effect variable, `Inferred`, or `Error` placeholder. The
    /// canonical converter is the one keys use, so it accepts the one
    /// legitimate `Error`-charged shape, the effect-row encoding inside
    /// coroutine and task type arguments.
    fn check_plan_types(&self, diagnostics: &mut Vec<Diagnostic>) {
        for (id, artifact) in self.artifacts.iter() {
            let Some(plan) = &artifact.plan else {
                continue; // `validate_specializations` reports a missing plan.
            };
            let origin = &artifact.origin;
            let mut problems = Vec::new();
            plan.visit_types(&mut |value| {
                let result = match value {
                    PlanType::Value(value_type) => CanonicalType::concrete(value_type, origin)
                        .map(|_| ())
                        .map_err(|diagnostic| (value_type.to_string(), diagnostic)),
                    PlanType::Function(function_type) => {
                        CanonicalFunctionType::concrete(function_type, origin)
                            .map(|_| ())
                            .map_err(|diagnostic| {
                                (
                                    crate::CheckedType::Function(function_type.clone()).to_string(),
                                    diagnostic,
                                )
                            })
                    }
                    PlanType::Resource(resource) => {
                        CanonicalType::concrete(&resource.value_type, origin)
                            .map(|_| ())
                            .map_err(|diagnostic| (resource.value_type.to_string(), diagnostic))
                    }
                };
                if let Err(problem) = result {
                    problems.push(problem);
                }
            });
            for (rendered, problem) in problems {
                diagnostics.push(Diagnostic::new(
                    origin.span.clone(),
                    format!(
                        "{} artifact {} carries a type that is not concrete: `{rendered}` ({})",
                        plan.family_name(),
                        id.index(),
                        problem.message
                    ),
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use crate::{
        CheckedType, DropGlueBody, LoweredArtifactPlan, LoweredModule, Lowerer, NameResolver,
        ProgramLoader, TypeChecker,
    };

    use super::super::artifact_closure::{ArtifactUseSite, ProductionHooks};

    fn lower(source: &str) -> LoweredModule {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("compiler crate should have a workspace parent");
        let standard_library: PathBuf = root.join("stdlib");
        let program = ProgramLoader::new()
            .with_standard_library_root(standard_library)
            .load_source(source, root)
            .expect("test source should load");
        let resolved = NameResolver::new()
            .resolve_program(program)
            .expect("test source should resolve");
        let module = TypeChecker::new()
            .check(resolved)
            .expect("test source should type check");
        Lowerer::new()
            .lower(&module)
            .unwrap_or_else(|diagnostics| panic!("source should lower: {diagnostics:?}"))
    }

    /// Two discarded `CString` results in one body: two `DiscardedResult`
    /// sites that request the same drop glue with the same kind, plus an
    /// unrelated binding item.
    const TWO_DISCARDS: &str = concat!(
        "use std.cinterop.(CString, c_string)\n",
        "def make: () -> CString = () => c_string \"x\"\n",
        "def discard_twice: () -> () = () => {\n",
        "  let unrelated = 0\n",
        "  make ()\n",
        "  make ()\n",
        "  ()\n",
        "}\n",
        "let done = discard_twice ()\n",
    );

    #[test]
    fn initializer_names_are_unique_and_revalidated() {
        let mut lowered = lower(concat!(
            "let a: I32 = { mod foo { pub let value: I32 = 1 }; foo.value }\n",
            "let b: I32 = { mod foo { pub let value: I32 = 2 }; foo.value }\n",
        ));
        let names = lowered
            .program
            .initializers
            .iter()
            .map(|(_, initializer)| initializer.name.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            names.iter().collect::<std::collections::HashSet<_>>().len(),
            names.len()
        );
        assert!(lowered.program.validate_closed_catalog().is_empty());
        lowered
            .program
            .initializers
            .iter_mut()
            .next()
            .expect("initializer")
            .1
            .name
            .clear();
        assert!(
            lowered
                .program
                .validate_closed_catalog()
                .iter()
                .any(|diagnostic| diagnostic.message.contains("initializer name disagrees"))
        );
    }

    #[test]
    fn a_plan_carrying_a_placeholder_type_is_diagnosed() {
        let mut lowered = lower(TWO_DISCARDS);
        let program = &mut lowered.program;
        assert!(
            program.validate_closed_catalog().is_empty(),
            "the closed catalog validates before corruption"
        );
        let (_, artifact) = program
            .artifacts
            .iter_mut()
            .find(|(_, artifact)| {
                matches!(
                    &artifact.plan,
                    Some(LoweredArtifactPlan::DropGlue(plan))
                        if plan.body == DropGlueBody::CStringFree
                )
            })
            .expect("the CString drop glue");
        let Some(LoweredArtifactPlan::DropGlue(plan)) = &mut artifact.plan else {
            unreachable!()
        };
        plan.value_type = CheckedType::Inferred;
        let diagnostics = program.validate_closed_catalog();
        assert!(
            diagnostics.iter().any(|diagnostic| diagnostic
                .message
                .contains("carries a type that is not concrete")),
            "an inferred placeholder in a plan is diagnosed: {diagnostics:?}"
        );

        let (_, artifact) = program
            .artifacts
            .iter_mut()
            .find(|(_, artifact)| matches!(&artifact.plan, Some(LoweredArtifactPlan::DropGlue(_))))
            .expect("a drop glue plan");
        let Some(LoweredArtifactPlan::DropGlue(plan)) = &mut artifact.plan else {
            unreachable!()
        };
        plan.value_type = CheckedType::Error;
        assert!(
            program
                .validate_closed_catalog()
                .iter()
                .any(|diagnostic| diagnostic
                    .message
                    .contains("carries a type that is not concrete")),
            "an error placeholder in a plan is diagnosed"
        );
    }

    #[test]
    fn a_use_moved_to_another_site_is_diagnosed() {
        let mut lowered = lower(TWO_DISCARDS);
        let program = &mut lowered.program;
        assert!(
            program
                .validate_artifact_closure(&ProductionHooks)
                .is_empty(),
            "the closure validates before corruption"
        );
        let (_, instance) = program
            .instances
            .iter_mut()
            .find(|(_, instance)| {
                instance.body.as_ref().is_some_and(|body| {
                    body.artifact_uses
                        .iter()
                        .filter(|use_| matches!(use_.site, ArtifactUseSite::DiscardedResult(_)))
                        .count()
                        == 2
                })
            })
            .expect("the body with two discarded results");
        let body = instance.body.as_mut().expect("materialized body");
        // Point the first discard's use at an item that is not a discard: the
        // item exists (so the arena check passes) and the edge is unchanged
        // (so edge agreement passes), but the site emission would emit the
        // drop from is wrong. Only the artifact planning exact-site check sees it.
        let discards = body
            .artifact_uses
            .iter()
            .filter_map(|use_| match use_.site {
                ArtifactUseSite::DiscardedResult(item) => Some(item),
                _ => None,
            })
            .collect::<Vec<_>>();
        let other = body
            .items
            .iter()
            .map(|(id, _)| id)
            .find(|id| !discards.contains(id))
            .expect("a non-discard item");
        let use_ = body
            .artifact_uses
            .iter_mut()
            .find(|use_| matches!(use_.site, ArtifactUseSite::DiscardedResult(_)))
            .expect("a discard use");
        use_.site = ArtifactUseSite::DiscardedResult(other);
        let diagnostics = program.validate_artifact_closure(&ProductionHooks);
        assert!(
            diagnostics.iter().any(|diagnostic| diagnostic
                .message
                .contains("is not bound to requested artifact")),
            "a use recorded at the wrong site is diagnosed: {diagnostics:?}"
        );
    }

    /// Every catalog fact emission consumes, rendered in catalog order: planned
    /// names, instances with their edges, uses, and owned bindings, artifacts
    /// with their request roots, edges, and plans, the initializers' uses,
    /// edges, and owned bindings, and the runtime requirement set.
    fn catalog_snapshot(program: &crate::LoweredProgram) -> String {
        use crate::ArenaId;
        let mut out = String::new();
        out.push_str(&format!(
            "names={:?}\n",
            program
                .specializations
                .planned_names_with(program.declared_name_resolver())
                .expect("unique planned names")
        ));
        for (id, instance) in program.instances.iter() {
            out.push_str(&format!(
                "instance {} ordinal={} name={} request={:?} deps={:?} artifacts={:?}\n",
                id.index(),
                instance.ordinal.index(),
                instance.name,
                instance.request,
                instance
                    .dependencies
                    .iter()
                    .map(|edge| (edge.instance.index(), edge.kind.description(), &edge.origin))
                    .collect::<Vec<_>>(),
                instance
                    .artifacts
                    .iter()
                    .map(|edge| (edge.artifact.index(), edge.kind.description(), &edge.origin))
                    .collect::<Vec<_>>(),
            ));
            if let Some(body) = &instance.body {
                out.push_str(&format!(
                    "  uses={:?} instance_uses={:?} owned={:?}\n",
                    body.artifact_uses
                        .iter()
                        .map(|use_| (use_.artifact.index(), use_.kind.description(), use_.site))
                        .collect::<Vec<_>>(),
                    body.instance_uses
                        .iter()
                        .map(|use_| (use_.instance.index(), use_.kind.description(), use_.site))
                        .collect::<Vec<_>>(),
                    body.owned_bindings,
                ));
            }
        }
        for (id, artifact) in program.artifacts.iter() {
            out.push_str(&format!(
                "artifact {} ordinal={} name={} root={:?} edges={:?} instances={:?} plan={:?}\n",
                id.index(),
                artifact.ordinal.index(),
                artifact.name,
                artifact.request,
                artifact
                    .artifacts
                    .iter()
                    .map(|edge| (edge.artifact.index(), edge.kind.description()))
                    .collect::<Vec<_>>(),
                artifact
                    .instances
                    .iter()
                    .map(|edge| (edge.instance.index(), edge.kind.description()))
                    .collect::<Vec<_>>(),
                artifact.plan,
            ));
        }
        for (id, _) in program.initializers.iter() {
            let index = id.index();
            out.push_str(&format!(
                "initializer {index} uses={:?} instance_uses={:?} artifacts={:?} instances={:?} owned={:?} bindings={:?} evidence={:?}\n",
                program.initializer_artifact_uses.get(index).map(|uses| uses
                    .iter()
                    .map(|use_| (use_.artifact.index(), use_.kind.description(), use_.site))
                    .collect::<Vec<_>>()),
                program.initializer_instance_uses.get(index).map(|uses| uses
                    .iter()
                    .map(|use_| (use_.instance.index(), use_.kind.description(), use_.site))
                    .collect::<Vec<_>>()),
                program.initializer_artifacts.get(index).map(|edges| edges
                    .iter()
                    .map(|edge| (edge.artifact.index(), edge.kind.description()))
                    .collect::<Vec<_>>()),
                program.initializer_instances.get(index).map(|edges| edges
                    .iter()
                    .map(|edge| (edge.instance.index(), edge.kind.description()))
                    .collect::<Vec<_>>()),
                program.initializer_owned_bindings.get(index),
                program.initializer_bindings.get(index).map(|bindings| bindings
                    .iter()
                    .map(|(site, target)| (*site, target.clone()))
                    .collect::<Vec<_>>()),
                program.initializer_evidence.get(index).map(|evidence| evidence
                    .iter()
                    .map(|(site, evidence)| (*site, evidence.clone()))
                    .collect::<Vec<_>>()),
            ));
        }
        for (_, id, symbol) in program.symbols.iter() {
            out.push_str(&format!(
                "symbol {} name={} storage={:?} module={} module_symbol={} has_global={} global_root={} overloaded={} flags=[mut={} captured={} non_owning={} derived={} signal={} mutated_param={} captured_cell={} external={}]\n",
                id.0,
                symbol.name,
                symbol.storage,
                symbol.module.0,
                symbol.module_symbol,
                symbol.has_global,
                symbol.global_root,
                symbol.overloaded,
                symbol.mutable_storage,
                symbol.captured,
                symbol.non_owning,
                symbol.derived,
                symbol.signal,
                symbol.mutated_parameter,
                symbol.captured_cell,
                symbol.external,
            ));
        }
        out.push_str(&format!(
            "runtime={:?}\n",
            program.runtime_requirements.requirements()
        ));
        out
    }

    #[test]
    fn repeated_lowering_yields_identical_catalogs_and_plans() {
        // Each run loads, checks, and lowers from scratch; every `HashMap`
        // in the pipeline gets a fresh random seed, so iteration-order
        // dependence would show up as a snapshot difference.
        let source = concat!(
            "use std.coroutine.*\n",
            "use std.cinterop.(CString, c_string)\n",
            "use std.buffer.*\n",
            "extern \"c\" { inspect: CString -> I32 }\n",
            "type Resource = ctor I32\n",
            "impl Drop Resource { def drop = Resource value => () }\n",
            "let make_resource: () -> (Resource -> Ref Resource) = () => Ref\n",
            "def show_pair: (I32, I32) -> String = pair => \"${pair:?}\"\n",
            "def pick: Bool -> (CString | I32) = condition => when { condition => c_string \"a\", else => 1 }\n",
            "def capture: move CString -> (() -> I32) = move value => () => inspect value\n",
            "def peek: <T> T -> I32 = _ => 1\n",
            "def generic: <T where Copy T> T -> Coroutine{} I32 = value => coro { peek value; 1 }\n",
            "let signal flag = 0\n",
            "def waiting: () -> Coroutine{Reactive} () = () => coro {\n",
            "  let _ = await (until { flag >= 1 })\n",
            "  ()\n",
            "}\n",
            "let mut strings: Buffer CString = Buffer.with_capacity (2 satisfies USize)\n",
            "let shown = show_pair (1, 2)\n",
            "let picked = pick True\n",
            "let run = capture (c_string \"x\")\n",
            "let a: Coroutine{} I32 = generic 1\n",
            "let b: Coroutine{} I32 = generic (1 satisfies U8)\n",
            "let c = with Reactive = reactive_scope () { waiting () }\n",
            "let d = with Reactive = reactive_scope () { reaction { () } }\n",
            "let inspected = inspect\n",
            "let doubled = flag + flag\n",
        );
        let first = catalog_snapshot(&lower(source).program);
        for plan in [
            "ConstructorAdapter(",
            "StructuralMethod(",
            "DropGlue(",
            "GcFinalizer(",
            "CoroutineCodes(",
            "ReactionRunner(",
            "UntilRunner(",
            "DerivedRunner(",
            "ExternAdapter(",
        ] {
            assert!(
                first.contains(plan),
                "the snapshot fixture covers `{plan}` plans"
            );
        }
        for run in 0..2 {
            let again = catalog_snapshot(&lower(source).program);
            if again != first {
                let line = first
                    .lines()
                    .zip(again.lines())
                    .position(|(left, right)| left != right);
                panic!(
                    "repeated lowering {run} changed the catalog at line {line:?}:\nfirst:  {:?}\nagain:  {:?}",
                    line.and_then(|line| first.lines().nth(line)),
                    line.and_then(|line| again.lines().nth(line)),
                );
            }
        }
    }
}
