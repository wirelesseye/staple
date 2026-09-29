//! Stage 5.2: the backend-local calling-convention layer.
//!
//! These functions build the LLVM function types and argument classifications
//! both emitters share. They are parameterized by [`LayoutContext`] for the
//! `Copy` decisions that decide indirect parameter slots; they never consult
//! the checker or the `TypedModule`.

use inkwell::{AddressSpace, types::BasicTypeEnum};

use crate::{CheckedFunctionType, CheckedMutation, CheckedType};

use super::{Backend, CodeGenerationResult, Diagnostic, Span};

impl<'program, 'context> Backend<'program, 'context> {
    pub(crate) fn compile_native_function_type(
        &self,
        function_type: &CheckedFunctionType,
    ) -> CodeGenerationResult<inkwell::types::FunctionType<'context>> {
        let return_type = self.compile_type(&function_type.result)?;
        let parameter_types = self.compile_parameter_types(&function_type.parameter)?;
        let variadic = matches!(
            &*function_type.parameter,
            CheckedType::Product(product) if product.variadic
        );
        Ok(match return_type {
            BasicTypeEnum::ArrayType(value) => value.fn_type(&parameter_types, variadic),
            BasicTypeEnum::FloatType(value) => value.fn_type(&parameter_types, variadic),
            BasicTypeEnum::IntType(value) => value.fn_type(&parameter_types, variadic),
            BasicTypeEnum::PointerType(value) => value.fn_type(&parameter_types, variadic),
            BasicTypeEnum::StructType(value) => value.fn_type(&parameter_types, variadic),
            BasicTypeEnum::VectorType(_) | BasicTypeEnum::ScalableVectorType(_) => {
                return Err(Diagnostic::new(
                    Span::Compiler,
                    "vector return types are not supported",
                ));
            }
        })
    }

    pub(crate) fn compile_closure_function_type(
        &self,
        function_type: &CheckedFunctionType,
    ) -> CodeGenerationResult<inkwell::types::FunctionType<'context>> {
        let return_type = self.compile_type(&function_type.result)?;
        let mut parameter_types = vec![self.context.ptr_type(AddressSpace::default()).into()];
        for resource in &function_type.effects.resources {
            if resource.mutable || !self.layout.is_copy(&resource.value_type) {
                parameter_types.push(self.context.ptr_type(AddressSpace::default()).into());
            } else {
                parameter_types.push(self.compile_type(&resource.value_type)?.into());
            }
        }
        let value_parameters = if function_type.mutations.contains(&CheckedMutation::Whole) {
            vec![self.context.ptr_type(AddressSpace::default()).into()]
        } else {
            let mut parameters = self.compile_parameter_types(&function_type.parameter)?;
            let indirect_mask = self.indirect_parameter_mask(function_type);
            for (index, parameter) in parameters.iter_mut().enumerate() {
                if indirect_mask[index] {
                    *parameter = self.context.ptr_type(AddressSpace::default()).into();
                }
            }
            parameters
        };
        parameter_types.extend(value_parameters);
        Ok(match return_type {
            BasicTypeEnum::ArrayType(value) => value.fn_type(&parameter_types, false),
            BasicTypeEnum::FloatType(value) => value.fn_type(&parameter_types, false),
            BasicTypeEnum::IntType(value) => value.fn_type(&parameter_types, false),
            BasicTypeEnum::PointerType(value) => value.fn_type(&parameter_types, false),
            BasicTypeEnum::StructType(value) => value.fn_type(&parameter_types, false),
            BasicTypeEnum::VectorType(_) | BasicTypeEnum::ScalableVectorType(_) => {
                return Err(Diagnostic::new(
                    Span::Compiler,
                    "vector return types are not supported",
                ));
            }
        })
    }

    pub(crate) fn compile_parameter_types(
        &self,
        parameter_type: &CheckedType,
    ) -> CodeGenerationResult<Vec<inkwell::types::BasicMetadataTypeEnum<'context>>> {
        match parameter_type {
            CheckedType::Product(product) => product
                .elements
                .iter()
                .map(|element| self.compile_type(&element.value_type).map(Into::into))
                .collect(),
            other => Ok(vec![self.compile_type(other)?.into()]),
        }
    }

    /// Which flattened parameters pass indirectly: a `mut`/moved parameter, or
    /// a parameter the catalog does not consider `Copy`.
    pub(crate) fn indirect_parameter_mask(&self, function_type: &CheckedFunctionType) -> Vec<bool> {
        let types = flattened_parameter_types(&function_type.parameter);
        let mutation_mask = mutation_parameter_mask(types.len(), &function_type.mutations);
        let move_mask = mutation_parameter_mask(types.len(), &function_type.moves);
        types
            .iter()
            .enumerate()
            .map(|(index, value_type)| {
                mutation_mask[index] || (!move_mask[index] && !self.layout.is_copy(value_type))
            })
            .collect()
    }

    /// Builds a function type with the given return and parameter types,
    /// defaulting a vector return to `void` (as the closure ABI does).
    pub(crate) fn build_fn_type(
        &self,
        return_type: BasicTypeEnum<'context>,
        parameters: &[inkwell::types::BasicMetadataTypeEnum<'context>],
    ) -> inkwell::types::FunctionType<'context> {
        match return_type {
            BasicTypeEnum::ArrayType(value) => value.fn_type(parameters, false),
            BasicTypeEnum::FloatType(value) => value.fn_type(parameters, false),
            BasicTypeEnum::IntType(value) => value.fn_type(parameters, false),
            BasicTypeEnum::PointerType(value) => value.fn_type(parameters, false),
            BasicTypeEnum::StructType(value) => value.fn_type(parameters, false),
            BasicTypeEnum::VectorType(_) | BasicTypeEnum::ScalableVectorType(_) => {
                self.context.void_type().fn_type(parameters, false)
            }
        }
    }
}

pub(crate) fn mutation_parameter_mask(count: usize, mutations: &[CheckedMutation]) -> Vec<bool> {
    let whole = mutations.contains(&CheckedMutation::Whole);
    (0..count)
        .map(|index| whole || mutations.contains(&CheckedMutation::Element(index)))
        .collect()
}

pub(crate) fn flattened_parameter_types(parameter: &CheckedType) -> Vec<&CheckedType> {
    match parameter {
        CheckedType::Product(product) => product
            .elements
            .iter()
            .map(|element| &element.value_type)
            .collect(),
        other => vec![other],
    }
}
