//! Stage 4.7: closed-catalog validation.
//!
//! Stages 4.2-4.6 already prove, inside `validate_specializations`,
//! `validate_specialization_graph`, and `validate_artifact_closure`, that every
//! artifact key rebuilds from its plan, keys and planned names are unique and
//! non-empty, every edge and request root names an existing owner of the
//! right family and kind, no `CompilerHelper` target survives, every planned
//! callee is bound, and one more closure round reserves nothing (including,
//! since Stage 4.7, that every re-scanned site is bound to its target at that
//! exact site). This module adds the remaining closed-catalog rule: every type
//! an artifact plan carries is fully concrete.

use staple_syntax::Diagnostic;

use super::{ArenaId, LoweredProgram, PlanType};
use crate::specialization::{CanonicalFunctionType, CanonicalType};

impl LoweredProgram {
    /// The closed-catalog checks Stage 4.7 adds on top of the Stage 4.2-4.6
    /// validators. Runs after `validate_artifact_closure`.
    pub(super) fn validate_closed_catalog(&self) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        self.check_plan_types(&mut diagnostics);
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
        // (so edge agreement passes), but the site Stage 5 would emit the
        // drop from is wrong. Only the Stage 4.7 exact-site check sees it.
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
}
