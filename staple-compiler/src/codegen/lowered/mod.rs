//! Parallel LLVM emitter over the read-only lowered program view.

use std::collections::HashMap;

use inkwell::{module::Module as LlvmModule, targets::TargetMachine, values::FunctionValue};

use crate::{EmissionView, FunctionInstanceId, RuntimeRequirement};

use super::{Backend, CodeGenerationResult, Diagnostic, LayoutContext};

pub(super) struct LoweredEmitter<'program, 'context> {
    view: EmissionView<'program>,
    backend: Backend<'program, 'context>,
    instances: HashMap<FunctionInstanceId, FunctionValue<'context>>,
}

impl<'program, 'context> LoweredEmitter<'program, 'context> {
    pub(super) fn new(
        context: &'context inkwell::context::Context,
        view: EmissionView<'program>,
        target_machine: &TargetMachine,
    ) -> Self {
        Self {
            view,
            backend: Backend::new(context, target_machine, LayoutContext::new(view)),
            instances: HashMap::new(),
        }
    }

    pub(super) fn compile(
        mut self,
        target_machine: &TargetMachine,
    ) -> CodeGenerationResult<LlvmModule<'context>> {
        self.backend
            .llvm_module
            .set_triple(&target_machine.get_triple());
        self.backend
            .llvm_module
            .set_data_layout(&target_machine.get_target_data().get_data_layout());

        // An executable always needs the collector for its entry harness.
        self.backend.install_gc_runtime()?;
        let requirements = self.view.runtime_requirements();
        if requirements.contains(RuntimeRequirement::ReactiveRuntime) {
            self.backend.install_reactive_runtime()?;
        }
        if requirements.contains(RuntimeRequirement::CoroutineRuntime) {
            self.backend.install_coroutine_runtime()?;
        }

        for (id, instance) in self.view.instances() {
            let Some(signature) = self.view.instance_signature(id) else {
                continue;
            };
            let name = self.view.planned_name(id).ok_or_else(|| {
                Diagnostic::new(
                    instance.origin.span.clone(),
                    "missing planned instance name",
                )
            })?;
            let function_type = self.backend.compile_closure_function_type(signature)?;
            let function = self
                .backend
                .llvm_module
                .add_function(name, function_type, None);
            self.instances.insert(id, function);
        }

        // The remaining owner families need instruction generation before the
        // module can verify. Report their first source origin, never a partial IR.
        if let Some((_, instance)) = self
            .view
            .instances()
            .find(|(_, instance)| instance.body.is_some())
        {
            return Err(Diagnostic::new(
                instance.origin.span.clone(),
                "lowered emitter: function bodies are not implemented yet",
            ));
        }
        Err(Diagnostic::new(
            staple_syntax::Span::Compiler,
            "lowered emitter: entry harness is not implemented yet",
        ))
    }
}
