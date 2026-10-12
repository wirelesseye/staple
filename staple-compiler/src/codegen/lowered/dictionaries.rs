//! Constant trait dictionaries. Every pointer is selected and bound by lowering.
use super::{CodeGenerationResult, Diagnostic, LoweredEmitter};
use crate::specialization::ArtifactOrdinal;
use inkwell::AddressSpace;

impl<'program, 'context> LoweredEmitter<'program, 'context> {
    pub(super) fn emit_trait_dictionary(
        &self,
        ordinal: ArtifactOrdinal,
        plan: &crate::TraitDictionaryPlan,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<()> {
        if plan.slots.len() != plan.layout.slots.len()
            || plan.prerequisites.len() != plan.layout.prerequisites.len()
            || plan.associated.len() != plan.layout.associated.len()
            || plan.methods.len() != plan.slots.len()
        {
            return Err(Diagnostic::new(
                span.clone(),
                "dictionary pointer count disagrees with its layout",
            ));
        }
        let pointer = self.backend.context.ptr_type(AddressSpace::default());
        let mut fields = Vec::new();
        for slot in &plan.slots {
            let ordinal = slot
                .artifact
                .ok_or_else(|| Diagnostic::new(span.clone(), "unbound dictionary slot"))?;
            let function = self
                .artifacts
                .get(&ordinal)
                .and_then(|functions| functions.first())
                .ok_or_else(|| Diagnostic::new(span.clone(), "missing dictionary slot entry"))?;
            fields.push(function.as_global_value().as_pointer_value());
        }
        for prerequisite in &plan.prerequisites {
            fields.push(match prerequisite {
                Some(prerequisite) => {
                    let ordinal = prerequisite.artifact.ok_or_else(|| {
                        Diagnostic::new(span.clone(), "unbound prerequisite dictionary")
                    })?;
                    self.descriptor_global(ordinal, span)?.as_pointer_value()
                }
                None => pointer.const_null(),
            });
        }
        for associated in &plan.associated {
            let ordinal = associated
                .artifact
                .ok_or_else(|| Diagnostic::new(span.clone(), "unbound dictionary descriptor"))?;
            fields.push(self.descriptor_global(ordinal, span)?.as_pointer_value());
        }
        // Static methods are concrete closures without captured environments.
        // Each slot thunk addresses its own pair through the dictionary pointer.
        for method in &plan.methods {
            let instance = method
                .instance
                .ok_or_else(|| Diagnostic::new(span.clone(), "unbound dictionary method"))?;
            let function = self.instances.get(&instance).ok_or_else(|| {
                Diagnostic::new(span.clone(), "missing concrete dictionary method")
            })?;
            fields.push(function.as_global_value().as_pointer_value());
            fields.push(pointer.const_null());
        }
        self.descriptor_global(ordinal, span)?
            .set_initializer(&pointer.const_array(&fields));
        Ok(())
    }
}
