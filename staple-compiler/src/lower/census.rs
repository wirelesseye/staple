//! Shared legacy declaration census for transition tests and shadow emission.
use super::LoweredProgram;
use crate::specialization::{
    ArtifactOrdinal, ArtifactRequestKey, ArtifactSite, ArtifactSiteOwner, CanonicalFunctionType,
    CanonicalType, GcFinalizerKey,
};
use crate::{FunctionInstanceId, LoweredArtifactPlan, ModuleId, Origin, TypeParameterId};
use std::collections::HashSet;

pub(crate) fn canonical(value_type: &crate::CheckedType) -> CanonicalType {
    CanonicalType::concrete(value_type, &Origin::compiler()).expect("a concrete type")
}

/// Whether one instance's concrete environment reproduces the legacy
/// recorded substitutions.
pub(crate) fn instance_substitutions_match(
    program: &LoweredProgram,
    instance: FunctionInstanceId,
    substitutions: &std::collections::HashMap<TypeParameterId, crate::CheckedType>,
) -> bool {
    let Some(instance) = program.instances.get(instance) else {
        return false;
    };
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
    types_match && effects_match
}

/// `instance_substitutions_match`, except that an effect parameter legacy
/// recorded no substitution for is accepted.
fn instance_substitutions_match_omitting_effects(
    program: &LoweredProgram,
    instance: FunctionInstanceId,
    substitutions: &std::collections::HashMap<TypeParameterId, crate::CheckedType>,
) -> bool {
    let Some(instance) = program.instances.get(instance) else {
        return false;
    };
    instance.relevant.type_parameters().all(|parameter| {
        matches!(
            (
                instance.environment.type_value(parameter),
                substitutions.get(&parameter),
            ),
            (Some(instance_value), Some(legacy)) if instance_value == legacy
        )
    }) && instance.relevant.effect_parameters().all(|parameter| {
        match substitutions.get(&parameter) {
            None => true,
            Some(legacy) => {
                instance.environment.effect_value(parameter)
                    == crate::effect_substitution_value(legacy)
            }
        }
    })
}

/// Every body thunk instance whose plan is a `CoroutineCodes` plan, keyed
/// by the `coro` body syntax the legacy cache uses.
pub(crate) fn pair_plans_by_syntax(
    program: &LoweredProgram,
) -> std::collections::HashMap<
    staple_syntax::SyntaxId,
    Vec<(FunctionInstanceId, &crate::CoroutineCodesPlan)>,
> {
    let mut by_syntax = std::collections::HashMap::new();
    for (id, instance) in program.instances.iter() {
        let Some(template) = program.functions.get(instance.template) else {
            continue;
        };
        if !template.class.coroutine_body {
            continue;
        }
        for (_, artifact) in program.artifacts.iter() {
            if let Some(crate::LoweredArtifactPlan::CoroutineCodes(plan)) = &artifact.plan
                && plan.body == id
            {
                by_syntax
                    .entry(template.body_syntax)
                    .or_insert_with(Vec::new)
                    .push((id, plan));
            }
        }
    }
    by_syntax
}

/// The owner's reactive operation arena for one runner owner.
pub(crate) fn runner_owner_operations<'a>(
    program: &'a LoweredProgram,
    owner: ArtifactSiteOwner,
) -> Vec<(
    crate::LoweredReactiveOperationId,
    &'a crate::LoweredReactiveOperation,
)> {
    match owner {
        ArtifactSiteOwner::Initializer(_) => program.reactive_operations.iter().collect(),
        ArtifactSiteOwner::Instance(ordinal) => program
            .instances
            .iter()
            .find(|(_, instance)| instance.ordinal == ordinal)
            .and_then(|(_, instance)| instance.body.as_ref())
            .map(|body| body.reactive_operations.iter().collect())
            .unwrap_or_default(),
        ArtifactSiteOwner::Artifact(_) => Vec::new(),
    }
}

/// The runner plans matching one legacy runner creation: same family,
/// the owner instance legacy was emitting (or any initializer), and a
/// site that resolves to the operation with the legacy call syntax or
/// evaluator. Shared by the Stage 4.5 comparison and the Stage 4.7
/// census.
pub(crate) fn runner_plans_for_legacy<'a>(
    program: &'a LoweredProgram,
    runner: &crate::codegen::LegacyReactiveRunner,
) -> Vec<(
    crate::specialization::ArtifactOrdinal,
    &'a crate::ReactiveRunnerPlan,
)> {
    use crate::LoweredArtifactPlan;
    use crate::codegen::LegacyRunnerFamily;
    let expected_owner = runner
        .owner
        .as_ref()
        .map(|(function, function_type, substitutions)| {
            let instance = program
                .instance_for_legacy_specialization(*function, function_type, substitutions)
                .unwrap_or_else(|| {
                    panic!(
                        "no instance matches the legacy runner's owner function {:?}",
                        function
                    )
                });
            program
                .instances
                .get(instance)
                .expect("the owner instance")
                .ordinal
        });
    let syntax = runner.call_syntax;
    let evaluator = runner.evaluator;
    program
        .artifacts
        .iter()
        .filter_map(|(_, artifact)| {
            let ordinal = artifact.ordinal;
            let plan = match artifact.plan.as_ref()? {
                LoweredArtifactPlan::ReactionRunner(plan)
                    if runner.family == LegacyRunnerFamily::Reaction =>
                {
                    plan
                }
                LoweredArtifactPlan::UntilRunner(plan)
                    if runner.family == LegacyRunnerFamily::Until =>
                {
                    plan
                }
                LoweredArtifactPlan::DerivedRunner(plan)
                    if runner.family == LegacyRunnerFamily::Derived =>
                {
                    plan
                }
                _ => return None,
            };
            let owner_ok = match (expected_owner, plan.owner) {
                (Some(ordinal), ArtifactSiteOwner::Instance(owner)) => owner == ordinal,
                (None, ArtifactSiteOwner::Initializer(_)) => true,
                _ => false,
            };
            if !owner_ok {
                return None;
            }
            let operations = runner_owner_operations(program, plan.owner);
            let site_ok = match (runner.family, plan.site) {
                (LegacyRunnerFamily::Reaction, ArtifactSite::Callback(callback)) => {
                    operations.iter().any(|(_, operation)| {
                        matches!(
                            &operation.kind,
                            crate::LoweredReactiveOperationKind::Reaction {
                                callback: recorded,
                                ..
                            } if *recorded == callback && operation.origin.syntax == syntax.unwrap_or(staple_syntax::SyntaxId(usize::MAX))
                        )
                    })
                }
                (LegacyRunnerFamily::Until, ArtifactSite::Callback(predicate)) => {
                    operations.iter().any(|(_, operation)| {
                        matches!(
                            &operation.kind,
                            crate::LoweredReactiveOperationKind::Until {
                                predicate: recorded,
                                ..
                            } if *recorded == predicate && operation.origin.syntax == syntax.unwrap_or(staple_syntax::SyntaxId(usize::MAX))
                        )
                    })
                }
                (LegacyRunnerFamily::Derived, ArtifactSite::Operation(operation)) => {
                    operations.iter().find(|(id, _)| *id == operation).is_some_and(
                        |(_, operation)| {
                            matches!(
                                &operation.kind,
                                crate::LoweredReactiveOperationKind::DerivedCreate {
                                    evaluator: recorded,
                                    ..
                                } if *recorded == evaluator.unwrap_or(crate::FunctionId(usize::MAX))
                            )
                        },
                    )
                }
                _ => false,
            };
            site_ok.then_some((ordinal, plan))
        })
        .collect::<Vec<_>>()
}

pub(crate) fn canonical_arguments(arguments: &[crate::CheckedType]) -> Vec<CanonicalType> {
    let origin = Origin::compiler();
    arguments
        .iter()
        .map(|argument| {
            CanonicalType::concrete(argument, &origin).expect("concrete legacy argument")
        })
        .collect()
}

/// What one census run accounted for, by legacy origin and by the
/// explained differences.
#[derive(Debug, Default)]
pub(crate) struct CensusCoverage {
    pub(crate) origins: std::collections::BTreeSet<&'static str>,
    pub(crate) functions: usize,
    pub(crate) explained: std::collections::BTreeSet<&'static str>,
}

impl CensusCoverage {
    pub(crate) fn merge(&mut self, other: CensusCoverage) {
        self.origins.extend(other.origins);
        self.functions += other.functions;
        self.explained.extend(other.explained);
    }
}

/// Stage 5.3 Step 3: one legacy-defined function's catalog mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LegacyCatalogEntry {
    /// A source-function instance.
    Instance(FunctionInstanceId),
    /// A generated artifact function. A coroutine pair maps to its
    /// `resume` or `cleanup` slot; every other family maps to `Single`.
    Artifact {
        ordinal: ArtifactOrdinal,
        slot: ArtifactSlot,
    },
    /// A module initializer.
    Initializer(ModuleId),
    /// The entry harness.
    Main,
    /// The fixed UTF-8 validator.
    Utf8Validator,
}

/// Which planned name of an artifact one legacy function uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArtifactSlot {
    Single,
    Resume,
    Cleanup,
}

/// Stage 4.7's forward census, kept reusable for the Stage 5.3
/// declaration-parity comparison: every legacy-defined function mapped to
/// the catalog entry that plans it, the explained negatives, and the
/// entries reachable only through a syntax-aliased coroutine.
#[derive(Debug, Default)]
pub(crate) struct CensusMapping {
    /// `(legacy final name, catalog entry)` for every mapped legacy
    /// function. `main` and the UTF-8 validator are included; an eager but
    /// unused extern adapter is explained instead.
    pub(crate) mapped: Vec<(String, LegacyCatalogEntry)>,
    /// Every mapped legacy function, including the specialization
    /// duplicates that resolve to an already-mapped eager instance. The
    /// body comparison uses this to rename legacy call targets.
    pub(crate) symbol_names: std::collections::HashMap<String, LegacyCatalogEntry>,
    pub(crate) instances: HashSet<FunctionInstanceId>,
    pub(crate) artifacts: HashSet<ArtifactOrdinal>,
    /// Lowered entries legacy never emitted because their coroutine pair
    /// was syntax-aliased to another instantiation. The lowered module
    /// does define them.
    pub(crate) aliased_instances: HashSet<FunctionInstanceId>,
    pub(crate) aliased_artifacts: HashSet<ArtifactOrdinal>,
    pub(crate) coverage: CensusCoverage,
    pub(crate) unexplained: Vec<String>,
}

/// Whether one coroutine pair plan's body syntax is a syntax legacy
/// emitted for another instantiation (Stage 4.5's syntax-keyed cache).
pub(crate) fn aliased_coroutine_pair(
    program: &crate::LoweredProgram,
    legacy: &crate::codegen::LegacyEmissions,
    plan: &crate::CoroutineCodesPlan,
) -> bool {
    program
        .instances
        .get(plan.body)
        .and_then(|instance| program.functions.get(instance.template))
        .is_some_and(|template| {
            legacy
                .coroutine_pairs
                .iter()
                .any(|pair| pair.body_syntax == template.body_syntax)
        })
}

/// Stage 4.7's forward census for one already-lowered program. Every
/// function the legacy module defines (outside the installed runtime
/// modules) is registered exactly once with the record it belongs to, and
/// each registered function maps to one catalog instance or artifact, or
/// to an explained negative reason. The reverse checks stay in
/// [`assert_census`]; the Stage 5.3 declaration comparison consumes the
/// mapping directly.
pub(crate) fn census_mapping(
    lowered: &crate::LoweredModule,
    legacy: &crate::codegen::LegacyEmissions,
) -> CensusMapping {
    use crate::codegen::{LegacyFinalizer, LegacyFunctionOrigin};
    use std::collections::{HashMap, HashSet};

    let program = &lowered.program;
    let origin = Origin::compiler();
    let mut mapping = CensusMapping::default();
    let mut mapped = Vec::new();
    let mut instances = HashSet::new();
    let mut artifacts = HashSet::new();
    // A non-generic function legacy also reaches through
    // `ensure_function_specialization` (the formatter helpers) has an
    // eager declaration and an internal specialization in the legacy
    // module. The catalog holds one instance for both; the eager
    // declaration is canonical, so the duplicate specialization mapping
    // is skipped (its linkage differs only because legacy emitted the
    // body twice).
    let mut eager_instances = HashSet::new();
    let mut duplicate_names = Vec::new();
    let mut coverage = CensusCoverage::default();
    let mut unexplained = Vec::new();

    // Completeness: every defined function has exactly one registration.
    let mut registered: HashMap<&str, Vec<&LegacyFunctionOrigin>> = HashMap::new();
    for (name, origin) in &legacy.defined_functions {
        registered.entry(name.as_str()).or_default().push(origin);
    }
    for name in &legacy.emitted_functions {
        match registered.get(name.as_str()).map(Vec::as_slice) {
            Some([_]) => {}
            Some(many) => unexplained.push(format!(
                "emitted function `{name}` is registered {} times",
                many.len()
            )),
            None => unexplained.push(format!("emitted function `{name}` is not registered")),
        }
    }
    let emitted = legacy
        .emitted_functions
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();

    // Forward: every emitted registration maps into the catalog.
    let artifact_where = |matches: &dyn Fn(&ArtifactRequestKey) -> bool| {
        program
            .artifacts
            .iter()
            .filter(|(_, artifact)| {
                program
                    .specializations
                    .artifact(artifact.ordinal)
                    .is_some_and(|key| matches(key))
            })
            .map(|(_, artifact)| artifact.ordinal)
            .collect::<Vec<_>>()
    };
    for (name, registration) in &legacy.defined_functions {
        // `main` and the UTF-8 validator are mapped but excluded from the
        // emitted-function set (legacy registers them without emitting
        // through the regular path).
        match registration {
            LegacyFunctionOrigin::Main => {
                mapped.push((name.clone(), LegacyCatalogEntry::Main));
                continue;
            }
            LegacyFunctionOrigin::Utf8Validator => {
                mapped.push((name.clone(), LegacyCatalogEntry::Utf8Validator));
                continue;
            }
            _ => {}
        }
        if !emitted.contains(name.as_str()) {
            // Declared without a body (a coroutine body thunk, compiled
            // into its `resume` instead) or excluded (`main`, the UTF-8
            // validator).
            continue;
        }
        coverage.functions += 1;
        let found: Result<(), String> = match registration {
            LegacyFunctionOrigin::Declared {
                function,
                function_type,
            } => {
                coverage.origins.insert("declared");
                program
                    .instance_for_legacy_specialization(*function, function_type, &HashMap::new())
                    .map(|instance| {
                        instances.insert(instance);
                        eager_instances.insert(instance);
                        mapped.push((name.clone(), LegacyCatalogEntry::Instance(instance)));
                    })
                    .ok_or_else(|| format!("declared function `{name}` has no instance"))
            }
            LegacyFunctionOrigin::Specialization {
                function,
                function_type,
                substitutions,
            } => {
                coverage.origins.insert("specialization");
                program
                    .instance_for_legacy_specialization(*function, function_type, substitutions)
                    .map(|instance| {
                        instances.insert(instance);
                        if eager_instances.contains(&instance) {
                            duplicate_names
                                .push((name.clone(), LegacyCatalogEntry::Instance(instance)));
                        } else {
                            mapped.push((name.clone(), LegacyCatalogEntry::Instance(instance)));
                        }
                    })
                    .ok_or_else(|| format!("specialization `{name}` has no instance"))
            }
            LegacyFunctionOrigin::Initializer(module) => {
                coverage.origins.insert("initializer");
                program
                    .initializers
                    .iter()
                    .any(|(_, initializer)| initializer.module == *module)
                    .then_some(())
                    .ok_or_else(|| format!("initializer `{name}` has no lowered initializer"))
                    .map(|()| {
                        mapped.push((name.clone(), LegacyCatalogEntry::Initializer(*module)));
                    })
            }
            LegacyFunctionOrigin::Main | LegacyFunctionOrigin::Utf8Validator => Ok(()),
            LegacyFunctionOrigin::ConstructorAdapter(index) => {
                coverage.origins.insert("constructor-adapter");
                let adapter = &legacy.constructor_adapters[*index];
                let callable_type =
                    CanonicalFunctionType::concrete(&adapter.callable_type, &origin)
                        .expect("concrete adapter type");
                match artifact_where(&|key| {
                    matches!(key, ArtifactRequestKey::ConstructorAdapter(key)
                        if key.symbol == adapter.symbol && key.callable_type == callable_type)
                })
                .as_slice()
                {
                    [ordinal] => {
                        artifacts.insert(*ordinal);
                        mapped.push((
                            name.clone(),
                            LegacyCatalogEntry::Artifact {
                                ordinal: *ordinal,
                                slot: ArtifactSlot::Single,
                            },
                        ));
                        Ok(())
                    }
                    other => Err(format!(
                        "constructor adapter `{name}` matches {} artifacts",
                        other.len()
                    )),
                }
            }
            LegacyFunctionOrigin::StructuralMethod(index) => {
                coverage.origins.insert("structural-method");
                let method = &legacy.structural_methods[*index];
                let arguments = canonical_arguments(&method.arguments);
                match artifact_where(&|key| {
                    matches!(key, ArtifactRequestKey::StructuralMethod(key)
                        if key.structural == method.structural && key.arguments == arguments)
                })
                .as_slice()
                {
                    [ordinal] => {
                        artifacts.insert(*ordinal);
                        mapped.push((
                            name.clone(),
                            LegacyCatalogEntry::Artifact {
                                ordinal: *ordinal,
                                slot: ArtifactSlot::Single,
                            },
                        ));
                        Ok(())
                    }
                    other => Err(format!(
                        "structural method `{name}` matches {} artifacts",
                        other.len()
                    )),
                }
            }
            LegacyFunctionOrigin::Finalizer(index) => {
                coverage.origins.insert("finalizer");
                let key = match &legacy.finalizers[*index] {
                    LegacyFinalizer::Payload(value_type) => {
                        Some(GcFinalizerKey::Payload(canonical(value_type)))
                    }
                    LegacyFinalizer::Cell(value_type) => {
                        Some(GcFinalizerKey::Cell(canonical(value_type)))
                    }
                    LegacyFinalizer::Buffer(element) => {
                        Some(GcFinalizerKey::Buffer(canonical(element)))
                    }
                    LegacyFinalizer::ClosureEnvironment {
                        function,
                        capture_types,
                        ..
                    } => {
                        let captures = capture_types.iter().map(canonical).collect::<Vec<_>>();
                        program
                            .instances
                            .iter()
                            .find(|(_, instance)| {
                                instance.template == *function
                                    && instance.body.as_ref().is_some_and(|body| {
                                        body.captures()
                                            .iter()
                                            .map(|capture| canonical(&capture.value_type))
                                            .collect::<Vec<_>>()
                                            == captures
                                    })
                            })
                            .map(|(_, instance)| GcFinalizerKey::ClosureEnvironment {
                                closure: instance.ordinal,
                                captures: captures.clone(),
                            })
                    }
                };
                key.and_then(|key| {
                    program
                        .specializations
                        .artifact_ordinal(&ArtifactRequestKey::GcFinalizer(key))
                })
                .map(|ordinal| {
                    artifacts.insert(ordinal);
                    mapped.push((
                        name.clone(),
                        LegacyCatalogEntry::Artifact {
                            ordinal,
                            slot: ArtifactSlot::Single,
                        },
                    ));
                })
                .ok_or_else(|| format!("finalizer `{name}` has no artifact"))
            }
            LegacyFunctionOrigin::CoroutineResume(index)
            | LegacyFunctionOrigin::CoroutineCleanup(index) => {
                coverage.origins.insert("coroutine-pair");
                let slot = if matches!(registration, LegacyFunctionOrigin::CoroutineResume(_)) {
                    ArtifactSlot::Resume
                } else {
                    ArtifactSlot::Cleanup
                };
                let pair = &legacy.coroutine_pairs[*index];
                pair_plans_by_syntax(program)
                    .get(&pair.body_syntax)
                    .and_then(|candidates| {
                        candidates
                            .iter()
                            .find(|(id, _)| {
                                instance_substitutions_match(program, *id, &pair.substitutions)
                            })
                            // Legacy's body cache records no effect
                            // substitution for a body that only reaches an
                            // effect parameter through its enclosing
                            // signature; the lowered instance still records
                            // the effect it was specialized with.
                            .or_else(|| {
                                candidates.iter().find(|(id, _)| {
                                    instance_substitutions_match_omitting_effects(
                                        program,
                                        *id,
                                        &pair.substitutions,
                                    )
                                })
                            })
                    })
                    .and_then(|(id, _)| {
                        let ordinal = program.instances.get(*id)?.ordinal;
                        program.specializations.artifact_ordinal(
                            &ArtifactRequestKey::CoroutineCodes(
                                crate::specialization::CoroutineCodesKey { body: ordinal },
                            ),
                        )
                    })
                    .map(|ordinal| {
                        artifacts.insert(ordinal);
                        mapped.push((name.clone(), LegacyCatalogEntry::Artifact { ordinal, slot }));
                    })
                    .ok_or_else(|| {
                        format!(
                            "coroutine function `{name}` has no pair plan (syntax {:?}, substitutions {:?}, candidates {:?})",
                            pair.body_syntax,
                            pair.substitutions,
                            pair_plans_by_syntax(program)
                                .get(&pair.body_syntax)
                                .map(|c| c
                                    .iter()
                                    .map(|(id, _)| {
                                        let i = program.instances.get(*id).unwrap();
                                        (
                                            i.relevant.type_parameters().collect::<Vec<_>>(),
                                            i.environment.clone(),
                                        )
                                    })
                                    .collect::<Vec<_>>()),
                        )
                    })
            }
            LegacyFunctionOrigin::Runner(index) => {
                coverage.origins.insert("runner");
                match runner_plans_for_legacy(program, &legacy.runners[*index]).as_slice() {
                    [(ordinal, _)] => {
                        artifacts.insert(*ordinal);
                        mapped.push((
                            name.clone(),
                            LegacyCatalogEntry::Artifact {
                                ordinal: *ordinal,
                                slot: ArtifactSlot::Single,
                            },
                        ));
                        Ok(())
                    }
                    other => Err(format!("runner `{name}` matches {} plans", other.len())),
                }
            }
            LegacyFunctionOrigin::ExternAdapter(index) => {
                coverage.origins.insert("extern-adapter");
                let adapter = &legacy.extern_adapters[*index];
                let callable_type =
                    CanonicalFunctionType::concrete(&adapter.callable_type, &origin)
                        .expect("concrete extern type");
                match artifact_where(&|key| {
                    matches!(key, ArtifactRequestKey::ExternAdapter(key)
                        if key.symbol == adapter.symbol && key.callable_type == callable_type)
                })
                .as_slice()
                {
                    [ordinal] => {
                        artifacts.insert(*ordinal);
                        mapped.push((
                            name.clone(),
                            LegacyCatalogEntry::Artifact {
                                ordinal: *ordinal,
                                slot: ArtifactSlot::Single,
                            },
                        ));
                        Ok(())
                    }
                    // Legacy declares every non-variadic extern's adapter
                    // eagerly; the catalog holds only reachable ones.
                    [] if !adapter.used => {
                        coverage.explained.insert("eager-unused-extern-adapter");
                        Ok(())
                    }
                    other => Err(format!(
                        "extern adapter `{name}` matches {} artifacts",
                        other.len()
                    )),
                }
            }
        };
        if let Err(problem) = found {
            unexplained.push(problem);
        }
    }

    // The syntax-keyed legacy cache emitted one pair for several
    // instantiations of the same `coro` body (Stage 4.5). An unmatched
    // pair whose body syntax legacy did emit is that alias, and anything
    // reachable only through it (its capture finalizer, the
    // instantiation-specific callees of its body) was never emitted.
    let aliased_pair =
        |plan: &crate::CoroutineCodesPlan| aliased_coroutine_pair(program, legacy, plan);
    let artifact_record = |ordinal| {
        program
            .artifacts
            .iter()
            .find(|(_, artifact)| artifact.ordinal == ordinal)
            .map(|(_, artifact)| artifact)
    };
    let mut aliased_instances = HashSet::new();
    let mut aliased_artifacts = HashSet::new();
    let mut pending_instances = Vec::new();
    let mut pending_artifacts = Vec::new();
    for (_, artifact) in program.artifacts.iter() {
        if let Some(LoweredArtifactPlan::CoroutineCodes(plan)) = &artifact.plan
            && !artifacts.contains(&artifact.ordinal)
            && aliased_pair(plan)
        {
            pending_artifacts.push(artifact.ordinal);
            pending_instances.push(plan.body);
        }
    }
    while !pending_instances.is_empty() || !pending_artifacts.is_empty() {
        while let Some(id) = pending_instances.pop() {
            if instances.contains(&id) || !aliased_instances.insert(id) {
                continue;
            }
            if let Some(instance) = program.instances.get(id) {
                pending_instances.extend(instance.dependencies.iter().map(|edge| edge.instance));
                pending_artifacts.extend(instance.artifacts.iter().map(|edge| edge.artifact));
            }
        }
        while let Some(ordinal) = pending_artifacts.pop() {
            if artifacts.contains(&ordinal) || !aliased_artifacts.insert(ordinal) {
                continue;
            }
            if let Some(artifact) = artifact_record(ordinal) {
                pending_instances.extend(artifact.instances.iter().map(|edge| edge.instance));
                pending_artifacts.extend(artifact.artifacts.iter().map(|edge| edge.artifact));
            }
        }
    }

    // Every mapped legacy function name, including the specialization
    // duplicates collapsed onto an eager instance, so the body comparison
    // can rename legacy call targets to their planned names.
    let mut symbol_names = mapped
        .iter()
        .map(|(name, entry)| (name.clone(), *entry))
        .collect::<std::collections::HashMap<_, _>>();
    symbol_names.extend(duplicate_names);
    mapping.mapped = mapped;
    mapping.symbol_names = symbol_names;
    mapping.instances = instances;
    mapping.artifacts = artifacts;
    mapping.aliased_instances = aliased_instances;
    mapping.aliased_artifacts = aliased_artifacts;
    mapping.coverage = coverage;
    mapping.unexplained = unexplained;
    mapping
}

/// The planned emitted name (or names) of one mapped catalog entry.
pub(crate) fn planned_names_for(
    view: crate::EmissionView<'_>,
    entry: &LegacyCatalogEntry,
) -> Vec<String> {
    match entry {
        LegacyCatalogEntry::Instance(instance) => vec![
            view.planned_name(*instance)
                .expect("mapped instance has a planned name")
                .to_owned(),
        ],
        LegacyCatalogEntry::Artifact { ordinal, slot } => match slot {
            ArtifactSlot::Single => vec![
                view.planned_artifact_name(*ordinal)
                    .expect("mapped artifact has a planned name")
                    .to_owned(),
            ],
            ArtifactSlot::Resume | ArtifactSlot::Cleanup => {
                let (resume, cleanup) = view
                    .planned_coroutine_pair_names(*ordinal)
                    .expect("a coroutine pair has both planned names");
                if *slot == ArtifactSlot::Resume {
                    vec![resume]
                } else {
                    vec![cleanup]
                }
            }
        },
        LegacyCatalogEntry::Initializer(module) => vec![
            view.initializers()
                .find(|(_, initializer)| initializer.module == *module)
                .expect("mapped initializer exists")
                .1
                .name
                .clone(),
        ],
        LegacyCatalogEntry::Main => vec!["main".to_owned()],
        LegacyCatalogEntry::Utf8Validator => vec!["__staple_is_valid_utf8".to_owned()],
    }
}

/// Stage 5.3 Step 3's declaration parity check for one already-lowered
/// program (the differential harness calls this too): the partial defined
/// set must equal the mapped legacy set plus `main`, the UTF-8 validator,
/// and the syntax-aliased entries, and every mapped pair must have
/// identical LLVM types and linkage.
pub(crate) fn assert_declaration_parity(
    label: &str,
    lowered: &crate::LoweredModule,
    legacy: &crate::codegen::LegacyEmissions,
    partial: &crate::codegen::LoweredEmissions,
) -> CensusMapping {
    let mapping = census_mapping(lowered, legacy);
    assert!(
        mapping.unexplained.is_empty(),
        "the census left functions unaccounted:\n{}\n{label}",
        mapping.unexplained.join("\n")
    );
    let program = &lowered.program;

    let runtime = crate::lower::worklist::runtime_module_symbols();
    let lowered_defined = partial
        .defined_functions
        .iter()
        .filter(|name| !runtime.contains(*name))
        .cloned()
        .collect::<HashSet<_>>();

    let mut expected_defined = HashSet::new();
    for (legacy_name, entry) in &mapping.mapped {
        let legacy_type = legacy
            .function_types
            .get(legacy_name)
            .unwrap_or_else(|| panic!("legacy `{legacy_name}` has no module type"));
        let legacy_linkage = legacy.function_linkages[legacy_name];
        for planned in planned_names_for(lowered.program(), entry) {
            let lowered_type = partial.function_types.get(&planned).unwrap_or_else(|| {
                panic!(
                    "the lowered module does not declare `{planned}` for legacy `{legacy_name}`\n{label}"
                )
            });
            assert_eq!(
                lowered_type, legacy_type,
                "LLVM type differs for legacy `{legacy_name}` -> `{planned}`\n{label}"
            );
            assert_eq!(
                partial.function_linkages[&planned], legacy_linkage,
                "linkage differs for legacy `{legacy_name}` -> `{planned}`\n{label}"
            );
            expected_defined.insert(planned);
        }
    }

    // Entries reachable only through a syntax-aliased coroutine are
    // additionally defined by the lowered module; legacy aliased them
    // away. Coroutine body thunks are compiled inside `resume`, so their
    // instance has no ordinary function (mirroring `declare_instances`).
    for instance in &mapping.aliased_instances {
        let record = program
            .instances
            .get(*instance)
            .expect("aliased instance is interned");
        let is_coroutine_body = program
            .functions
            .get(record.template)
            .is_some_and(|template| template.class.coroutine_body);
        if record.body.is_some() && !is_coroutine_body {
            expected_defined.insert(
                program
                    .planned_name(*instance)
                    .expect("aliased instance has a planned name")
                    .to_owned(),
            );
        }
    }
    for ordinal in &mapping.aliased_artifacts {
        let Some(record) = program
            .artifacts
            .iter()
            .find(|(_, artifact)| artifact.ordinal == *ordinal)
            .map(|(_, artifact)| artifact)
        else {
            continue;
        };
        match record.plan.as_ref() {
            Some(LoweredArtifactPlan::DropGlue(_)) | None => {}
            Some(LoweredArtifactPlan::CoroutineCodes(_)) => {
                let (resume, cleanup) = lowered
                    .program()
                    .planned_coroutine_pair_names(*ordinal)
                    .expect("a coroutine pair has both planned names");
                expected_defined.insert(resume);
                expected_defined.insert(cleanup);
            }
            Some(_) => {
                expected_defined.insert(
                    program
                        .planned_artifact_name(*ordinal)
                        .expect("aliased artifact has a planned name")
                        .to_owned(),
                );
            }
        }
    }

    assert_eq!(
        lowered_defined, expected_defined,
        "the lowered defined set differs from the census mapping\n{label}"
    );
    assert_catalog_census(label, lowered, partial);
    mapping
}

/// The legacy-free census every lowered module must pass. It needs only the
/// catalog and the emitted module:
///
/// - every function the module defines outside the runtime modules is the
///   planned name of exactly one catalog instance, artifact function, or
///   initializer, or is `main` or the UTF-8 validator;
/// - every catalog entry is defined, or explained: drop glue is inlined (D3),
///   a coroutine body thunk is emitted inside its pair's `resume`, and an
///   instance without a lowered body is declared but bodiless;
/// - each defined function's LLVM type and linkage equal the declaration the
///   catalog signature compiles to.
pub(crate) fn assert_catalog_census(
    label: &str,
    lowered: &crate::LoweredModule,
    emitted: &crate::codegen::LoweredEmissions,
) {
    let program = &lowered.program;
    let runtime = crate::lower::worklist::runtime_module_symbols();
    let defined = emitted
        .defined_functions
        .iter()
        .filter(|name| !runtime.contains(*name))
        .cloned()
        .collect::<HashSet<_>>();

    // Every planned name, with how many catalog entries claim it.
    let mut claims = std::collections::HashMap::<String, usize>::new();
    // The names the catalog says must be defined.
    let mut expected = HashSet::<String>::new();
    // The names the catalog declares (defined or explained bodiless).
    let mut declared = HashSet::<String>::new();
    let mut claim = |name: &str, defined: bool| {
        *claims.entry(name.to_owned()).or_default() += 1;
        declared.insert(name.to_owned());
        if defined {
            expected.insert(name.to_owned());
        }
    };
    for (id, record) in program.instances.iter() {
        let is_coroutine_body = program
            .functions
            .get(record.template)
            .is_some_and(|template| template.class.coroutine_body);
        if is_coroutine_body {
            // Emitted inside its pair's `resume`; it has no function of its own.
            continue;
        }
        let Some(name) = program.planned_name(id) else {
            continue;
        };
        claim(name, record.body.is_some());
    }
    for (_, artifact) in program.artifacts.iter() {
        match artifact.plan.as_ref() {
            // D3: drop glue is inlined at its use sites.
            Some(LoweredArtifactPlan::DropGlue(_)) | None => {}
            Some(LoweredArtifactPlan::CoroutineCodes(_)) => {
                let (resume, cleanup) = program
                    .planned_coroutine_pair_names(artifact.ordinal)
                    .expect("a coroutine pair has both planned names");
                claim(&resume, true);
                claim(&cleanup, true);
            }
            Some(_) => claim(
                program
                    .planned_artifact_name(artifact.ordinal)
                    .expect("an artifact function has a planned name"),
                true,
            ),
        }
    }
    for (_, initializer) in program.initializers.iter() {
        claim(&initializer.name, true);
    }

    let duplicated = claims
        .iter()
        .filter(|(_, count)| **count > 1)
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>();
    assert!(
        duplicated.is_empty(),
        "catalog entries share planned names {duplicated:?}\n{label}"
    );

    let mut allowed = expected.clone();
    allowed.insert("main".to_owned());
    allowed.insert("__staple_is_valid_utf8".to_owned());
    let mut unexplained = defined.difference(&allowed).cloned().collect::<Vec<_>>();
    unexplained.sort();
    assert!(
        unexplained.is_empty(),
        "the module defines functions no catalog entry plans: {unexplained:?}\n{label}"
    );
    let mut missing = expected.difference(&defined).cloned().collect::<Vec<_>>();
    missing.sort();
    assert!(
        missing.is_empty(),
        "catalog entries are neither defined nor explained: {missing:?}\n{label}"
    );
    assert!(
        defined.contains("main"),
        "every executable module defines `main`\n{label}"
    );

    // The declared type and linkage of every catalog function.
    let context = inkwell::context::Context::create();
    let catalog = crate::codegen::lowered_catalog_types(&context, lowered)
        .unwrap_or_else(|diagnostics| panic!("catalog declarations: {diagnostics:?}\n{label}"));
    for name in &declared {
        let (planned_type, planned_internal) = catalog
            .get(name)
            .unwrap_or_else(|| panic!("catalog function `{name}` has no declaration\n{label}"));
        let emitted_type = emitted
            .function_types
            .get(name)
            .unwrap_or_else(|| panic!("the module does not declare `{name}`\n{label}"));
        assert_eq!(
            emitted_type, planned_type,
            "`{name}` is emitted with a type other than its catalog signature\n{label}"
        );
        assert_eq!(
            emitted.function_linkages.get(name),
            Some(planned_internal),
            "`{name}` is emitted with a linkage other than its catalog declaration\n{label}"
        );
    }
}
