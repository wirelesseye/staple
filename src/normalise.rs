use std::collections::HashMap;

use crate::{
    Binding, BlockExpression, CallExpression, Expression, ExternBlock, FunctionDefinition,
    FunctionExpression, FunctionType, InferredType, Item, ListExpression, Module, NameExpression,
    Statement, Type,
};

pub struct Normaliser {
    fn_defs: Vec<FunctionDefinition>,
    top_stmts: Vec<Statement>,
    scopes: Vec<HashMap<String, usize>>,
    next_id: usize,
}

impl Normaliser {
    pub fn new() -> Self {
        Self {
            fn_defs: Vec::new(),
            top_stmts: Vec::new(),
            scopes: Vec::new(),
            next_id: 0,
        }
    }

    pub fn normalise(&mut self, module: &mut Module) {
        self.push_scope();
        for item in &mut module.items {
            self.normalise_item(item);
        }
        module.fn_decls = std::mem::take(&mut self.fn_defs);
        module.top_stmts = std::mem::take(&mut self.top_stmts);
        self.pop_scope();
    }

    fn normalise_item(&mut self, item: &mut Item) {
        match item {
            Item::ExternBlock(extern_block) => self.normalise_extern_block(extern_block),
            Item::TypeDeclaration(type_declaration) => (),
            Item::Statement(stmt) => {
                self.normalise_stmt(stmt);
                self.top_stmts.push(stmt.clone());
            }
        }
    }

    fn normalise_extern_block(&mut self, extern_block: &mut ExternBlock) {
        for binding in &mut extern_block.bindings {
            binding.symbol_id = Some(self.declare(binding.name.clone()))
        }
    }

    fn normalise_stmt(&mut self, stmt: &mut Statement) {
        match stmt {
            Statement::Binding(binding) => self.normalise_binding(binding),
            Statement::Expression(expr) => self.normalise_expr(expr),
        }
    }

    fn normalise_binding(&mut self, binding: &mut Binding) {
        if let None = binding.symbol_id {
            binding.symbol_id = Some(self.declare(binding.name.clone()));
        }
        if let Some(value) = &mut binding.value {
            self.normalise_expr(value);
        }
    }

    fn normalise_expr(&mut self, expr: &mut Expression) {
        match expr {
            Expression::Function(fn_expr) => self.normalise_fn_expr(fn_expr),
            Expression::Block(block_expr) => self.normalise_block_expr(block_expr),
            Expression::List(list_expr) => self.normalise_list_expr(list_expr),
            Expression::Call(call_expr) => self.normalise_call_expr(call_expr),
            Expression::Access(access_expression) => (),
            Expression::Binary(binary_expression) => (),
            Expression::Name(name_expr) => self.normalise_name_expr(name_expr),
            Expression::String(string_expression) => (),
            Expression::Integer(integer_expression) => (),
        }
    }

    fn normalise_fn_expr(&mut self, fn_expr: &mut FunctionExpression) {
        self.push_scope();
        let fn_id = self.add_fn_def(FunctionDefinition {
            parameter: fn_expr.parameter.clone(),
            body: fn_expr.body.clone(),
            ty: FunctionType {
                syntax: fn_expr.syntax.clone(),
                parameter: Box::new(fn_expr.parameter.ty()),
                result: Box::new(Type::Inferred(InferredType::new())),
            },
        });
        fn_expr.fn_id = Some(fn_id);
        self.normalise_expr(&mut fn_expr.body);
        self.pop_scope();
    }

    fn normalise_block_expr(&mut self, block_expr: &mut BlockExpression) {
        self.push_scope();
        for stmt in &mut block_expr.statements {
            self.normalise_stmt(stmt);
        }
        self.pop_scope();
    }

    fn normalise_list_expr(&mut self, list_expr: &mut ListExpression) {
        for element in &mut list_expr.elements {
            self.normalise_expr(&mut element.value);
        }
    }

    fn normalise_call_expr(&mut self, call_expr: &mut CallExpression) {
        self.normalise_expr(&mut call_expr.callee);
        self.normalise_expr(&mut call_expr.argument);
    }

    fn normalise_name_expr(&mut self, name_expr: &mut NameExpression) {
        let symbol_id = self.lookup(&name_expr.name).unwrap();
        name_expr.symbol_id = Some(symbol_id);
    }

    fn fresh_symbol(&mut self) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn lookup(&self, name: &str) -> Option<usize> {
        for scope in self.scopes.iter().rev() {
            if let Some(symbol) = scope.get(name) {
                return Some(*symbol);
            }
        }

        None
    }

    fn declare(&mut self, name: String) -> usize {
        // Create the ID before taking the mutable borrow.
        let symbol = self.fresh_symbol();

        let scope = self.scopes.last_mut().expect("resolver always has a scope");

        scope.insert(name, symbol);
        symbol
    }

    fn add_fn_def(&mut self, fn_def: FunctionDefinition) -> usize {
        self.fn_defs.push(fn_def);
        self.fn_defs.len() - 1
    }
}
