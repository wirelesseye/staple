use std::collections::HashMap;

use inkwell::{
    AddressSpace,
    values::{AnyValue, AnyValueEnum, BasicValueEnum},
};

use crate::{
    Binding, BlockExpression, CallExpression, Expression, ExternBlock, FunctionDefinition,
    FunctionExpression, FunctionType, Item, ListExpression, ListType, Module, NameExpression,
    PrimitiveType, Statement, StringExpression, Type,
};

pub struct CodeGen<'ctx> {
    context: &'ctx inkwell::context::Context,
    builder: inkwell::builder::Builder<'ctx>,
}

struct ModuleContext<'ctx> {
    functions: Vec<inkwell::values::FunctionValue<'ctx>>,
    values: HashMap<usize, inkwell::values::AnyValueEnum<'ctx>>,
    types: HashMap<usize, inkwell::types::BasicTypeEnum<'ctx>>,
}

impl<'ctx> CodeGen<'ctx> {
    pub fn new(context: &'ctx inkwell::context::Context) -> Self {
        let builder = context.create_builder();

        Self { context, builder }
    }

    pub fn compile_module(&self, module: &Module) -> String {
        let llvm = self.context.create_module("example");
        let mut ctx = ModuleContext {
            functions: Vec::new(),
            values: HashMap::new(),
            types: HashMap::new(),
        };

        for item in &module.items {
            self.compile_item(&llvm, &mut ctx, item);
        }

        for fn_decl in &module.fn_decls {
            self.compile_fn_decl(&llvm, &mut ctx, fn_decl)
        }

        self.compile_main_fn(&llvm, &mut ctx, &module.top_stmts);

        llvm.verify().unwrap();
        llvm.print_to_string().to_string()
    }

    fn compile_fn_decl(
        &'ctx self,
        llvm: &inkwell::module::Module<'ctx>,
        ctx: &mut ModuleContext<'ctx>,
        fn_decl: &FunctionDefinition,
    ) {
        let fn_type = self.compile_fn_type(llvm, ctx, &fn_decl.ty);
        let function = llvm.add_function("", fn_type, None);
        ctx.functions.push(function);
        let entry = self.context.append_basic_block(function, "entry");
        self.builder.position_at_end(entry);

        self.compile_expr(llvm, ctx, &fn_decl.body);
    }

    fn compile_main_fn(
        &'ctx self,
        llvm: &inkwell::module::Module<'ctx>,
        ctx: &mut ModuleContext<'ctx>,
        stmts: &[Statement],
    ) {
        let i32_type = self.context.i32_type();
        let main_type = i32_type.fn_type(&[], false);
        let main_fn = llvm.add_function("main", main_type, None);
        ctx.functions.push(main_fn);

        let main_entry = self.context.append_basic_block(main_fn, "entry");
        self.builder.position_at_end(main_entry);

        for stmt in stmts {
            self.compile_stmt(llvm, ctx, stmt);
        }

        let return_value = i32_type.const_int(0, false);
        self.builder.build_return(Some(&return_value)).unwrap();
    }

    fn compile_stmt(
        &'ctx self,
        llvm: &inkwell::module::Module<'ctx>,
        ctx: &mut ModuleContext<'ctx>,
        stmt: &Statement,
    ) -> Option<inkwell::values::AnyValueEnum<'ctx>> {
        match stmt {
            Statement::Binding(binding) => {
                self.compile_binding(llvm, ctx, binding);
                None
            }
            Statement::Expression(expr) => Some(self.compile_expr(llvm, ctx, expr)),
        }
    }

    fn compile_binding(
        &'ctx self,
        llvm: &inkwell::module::Module<'ctx>,
        ctx: &mut ModuleContext<'ctx>,
        binding: &Binding,
    ) {
        if let Some(expr) = &binding.value {
            let symbol_id = binding.symbol_id.unwrap();
            let value = self.compile_expr(llvm, ctx, expr);
            ctx.values.insert(symbol_id, value);
        } else {
        }
    }

    fn compile_expr(
        &'ctx self,
        llvm: &inkwell::module::Module<'ctx>,
        ctx: &mut ModuleContext<'ctx>,
        expr: &Expression,
    ) -> inkwell::values::AnyValueEnum<'_> {
        match expr {
            Expression::Function(fn_expr) => self.compile_fn_expr(llvm, ctx, fn_expr).into(),
            Expression::Block(block_expr) => self.compile_block_expr(llvm, ctx, block_expr),
            Expression::List(list_expr) => self.compile_list_expr(llvm, ctx, list_expr).into(),
            Expression::Call(call_expr) => self
                .compile_call_expr(llvm, ctx, call_expr)
                .as_any_value_enum(),
            Expression::Access(access_expr) => todo!(),
            Expression::Binary(binary_expr) => todo!(),
            Expression::Name(name_expr) => self.compile_name_expr(llvm, ctx, name_expr),
            Expression::String(string_expr) => {
                self.compile_string_expr(string_expr).as_any_value_enum()
            }
            Expression::Integer(integer_expr) => todo!(),
        }
    }

    fn compile_fn_expr(
        &self,
        _llvm: &inkwell::module::Module<'ctx>,
        ctx: &mut ModuleContext<'ctx>,
        fn_expr: &FunctionExpression,
    ) -> inkwell::values::FunctionValue<'_> {
        let decl_id = fn_expr.fn_id.unwrap();
        ctx.functions.get(decl_id).cloned().unwrap()
    }

    fn compile_block_expr(
        &'ctx self,
        llvm: &inkwell::module::Module<'ctx>,
        ctx: &mut ModuleContext<'ctx>,
        block_expr: &BlockExpression,
    ) -> inkwell::values::AnyValueEnum<'_> {
        let mut value = None;
        for stmt in &block_expr.statements {
            value = self.compile_stmt(llvm, ctx, stmt);
        }
        value.unwrap_or(
            self.compile_list_expr(llvm, ctx, &ListExpression::empty())
                .into(),
        )
    }

    fn compile_list_expr(
        &'ctx self,
        llvm: &inkwell::module::Module<'ctx>,
        ctx: &mut ModuleContext<'ctx>,
        list_expr: &ListExpression,
    ) -> inkwell::values::StructValue<'_> {
        let struct_type = self.compile_list_type(llvm, ctx, list_expr.ty.as_ref().unwrap());
        let values = list_expr
            .elements
            .iter()
            .map(|element| self.compile_expr(llvm, ctx, &element.value))
            .map(any_value_into_basic_value)
            .collect::<Vec<_>>();
        struct_type.const_named_struct(&values)
    }

    fn compile_call_expr(
        &'ctx self,
        llvm: &inkwell::module::Module<'ctx>,
        ctx: &mut ModuleContext<'ctx>,
        call_expr: &CallExpression,
    ) -> inkwell::values::CallSiteValue {
        let callee = self.compile_expr(llvm, ctx, &call_expr.callee);
        let args = self.compile_argument(llvm, ctx, &call_expr.argument);
        match callee {
            AnyValueEnum::FunctionValue(function_value) => self
                .builder
                .build_direct_call(function_value, &args, "")
                .unwrap(),
            AnyValueEnum::PointerValue(pointer_value) => {
                let Type::Function(called_type) = call_expr.callee.ty().unwrap() else {
                    unreachable!()
                };
                let function_type = self.compile_fn_type(llvm, ctx, &called_type);
                self.builder
                    .build_indirect_call(function_type, pointer_value, &args, "")
                    .unwrap()
            }
            _ => unreachable!("not a callable"),
        }
    }

    fn compile_name_expr(
        &self,
        _llvm: &inkwell::module::Module<'ctx>,
        ctx: &mut ModuleContext<'ctx>,
        name_expr: &NameExpression,
    ) -> inkwell::values::AnyValueEnum<'_> {
        let symbol_id = name_expr
            .symbol_id
            .expect(&format!("{:?}", name_expr.syntax));
        ctx.values.get(&symbol_id).cloned().unwrap()
    }

    fn compile_string_expr(
        &self,
        string_expr: &StringExpression,
    ) -> inkwell::values::GlobalValue<'_> {
        self.builder
            .build_global_string_ptr(&string_expr.literal, "")
            .unwrap()
    }

    fn compile_argument(
        &'ctx self,
        llvm: &inkwell::module::Module<'ctx>,
        ctx: &mut ModuleContext<'ctx>,
        argument: &Expression,
    ) -> Vec<inkwell::values::BasicMetadataValueEnum> {
        match argument {
            Expression::List(list_expression) => list_expression
                .elements
                .iter()
                .map(|element| {
                    any_value_into_basic_value(self.compile_expr(llvm, ctx, &element.value)).into()
                })
                .collect(),
            _ => vec![any_value_into_basic_value(self.compile_expr(llvm, ctx, argument)).into()],
        }
    }

    fn compile_item(
        &'ctx self,
        llvm: &inkwell::module::Module<'ctx>,
        ctx: &mut ModuleContext<'ctx>,
        item: &Item,
    ) {
        match item {
            Item::ExternBlock(extern_block) => self.compile_extern_block(llvm, ctx, extern_block),
            Item::TypeDeclaration(_) => (),
            Item::Statement(_) => (), // Top-level statements are compiled in `compile_main_fn`
        }
    }

    fn compile_extern_block(
        &'ctx self,
        llvm: &inkwell::module::Module<'ctx>,
        ctx: &mut ModuleContext<'ctx>,
        extern_block: &ExternBlock,
    ) {
        for binding in &extern_block.bindings {
            let Some(Type::Function(fn_type)) = &binding.annotation else {
                unreachable!()
            };
            let function_type = self.compile_fn_type(llvm, ctx, fn_type);
            let function = llvm.add_function(&binding.name, function_type, None);
            ctx.values
                .insert(binding.symbol_id.unwrap(), function.into());
        }
    }

    fn compile_param_type(
        &self,
        llvm: &inkwell::module::Module<'ctx>,
        ctx: &mut ModuleContext<'ctx>,
        param_type: &Type,
    ) -> Vec<inkwell::types::BasicMetadataTypeEnum<'_>> {
        match param_type {
            Type::List(list_type) => list_type
                .elements
                .iter()
                .map(|element| self.compile_type(llvm, ctx, &element.ty).into())
                .collect(),
            _ => vec![self.compile_type(llvm, ctx, param_type).into()],
        }
    }

    fn compile_type(
        &self,
        llvm: &inkwell::module::Module<'ctx>,
        ctx: &mut ModuleContext<'ctx>,
        ty: &Type,
    ) -> inkwell::types::BasicTypeEnum<'_> {
        match ty {
            Type::Inferred(_) => unreachable!("inferred type"),
            Type::Named(named_type) => ctx
                .types
                .get(&named_type.symbol_id.unwrap())
                .cloned()
                .unwrap(),
            Type::Pointer(_) => self.context.ptr_type(AddressSpace::default()).into(),
            Type::List(list_type) => {
                let field_types = list_type
                    .elements
                    .iter()
                    .map(|element| self.compile_type(llvm, ctx, &element.ty).into())
                    .collect::<Vec<_>>();
                self.context.struct_type(&field_types, true).into()
            }
            Type::Function(_) => self.context.ptr_type(AddressSpace::default()).into(),
            Type::Primitive(primitive_type) => self.compile_primitive_type(primitive_type),
        }
    }

    fn compile_fn_type(
        &self,
        llvm: &inkwell::module::Module<'ctx>,
        ctx: &mut ModuleContext<'ctx>,
        fn_type: &FunctionType,
    ) -> inkwell::types::FunctionType<'_> {
        let return_type = self.compile_type(llvm, ctx, &fn_type.result);
        let param_types = self.compile_param_type(llvm, ctx, &fn_type.parameter);
        let is_var_args = match &*fn_type.parameter {
            Type::List(list_type) => list_type.variadic,
            _ => false,
        };

        match return_type {
            inkwell::types::BasicTypeEnum::ArrayType(array_type) => {
                array_type.fn_type(&param_types, is_var_args)
            }
            inkwell::types::BasicTypeEnum::FloatType(float_type) => {
                float_type.fn_type(&param_types, is_var_args)
            }
            inkwell::types::BasicTypeEnum::IntType(int_type) => {
                int_type.fn_type(&param_types, is_var_args)
            }
            inkwell::types::BasicTypeEnum::PointerType(pointer_type) => {
                pointer_type.fn_type(&param_types, is_var_args)
            }
            inkwell::types::BasicTypeEnum::StructType(struct_type) => {
                struct_type.fn_type(&param_types, is_var_args)
            }
            inkwell::types::BasicTypeEnum::VectorType(_) => unreachable!(),
            inkwell::types::BasicTypeEnum::ScalableVectorType(_) => unreachable!(),
        }
    }

    fn compile_list_type(
        &self,
        llvm: &inkwell::module::Module<'ctx>,
        ctx: &mut ModuleContext<'ctx>,
        list_type: &ListType,
    ) -> inkwell::types::StructType<'_> {
        let field_types = list_type
            .elements
            .iter()
            .map(|element| self.compile_type(llvm, ctx, &element.ty))
            .collect::<Vec<_>>();
        self.context.struct_type(&field_types, true)
    }

    fn compile_primitive_type(
        &self,
        primitive_type: &PrimitiveType,
    ) -> inkwell::types::BasicTypeEnum<'_> {
        match primitive_type {
            PrimitiveType::I32(_) => self.context.i32_type().into(),
            PrimitiveType::Bool(_) => self.context.bool_type().into(),
        }
    }
}

fn any_value_into_basic_value(any_value: AnyValueEnum) -> BasicValueEnum {
    match any_value {
        inkwell::values::AnyValueEnum::ArrayValue(array_value) => array_value.into(),
        inkwell::values::AnyValueEnum::IntValue(int_value) => int_value.into(),
        inkwell::values::AnyValueEnum::FloatValue(float_value) => float_value.into(),
        inkwell::values::AnyValueEnum::FunctionValue(function_value) => {
            function_value.as_global_value().as_pointer_value().into()
        }
        inkwell::values::AnyValueEnum::PointerValue(pointer_value) => pointer_value.into(),
        inkwell::values::AnyValueEnum::StructValue(struct_value) => struct_value.into(),
        inkwell::values::AnyValueEnum::VectorValue(vector_value) => vector_value.into(),
        _ => unreachable!(),
    }
}
