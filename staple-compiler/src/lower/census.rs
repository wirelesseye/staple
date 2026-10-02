//! Catalog definition, signature, linkage, and uniqueness checks.
use crate::specialization::CanonicalType;
use crate::{LoweredArtifactPlan, Origin};
use std::collections::HashSet;

pub(crate) fn canonical(value_type: &crate::CheckedType) -> CanonicalType {
    CanonicalType::concrete(value_type, &Origin::compiler()).expect("a concrete type")
}

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
            // drop glue is inlined at its use sites.
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
