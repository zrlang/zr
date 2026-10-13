//! 声明（declaration）解析：函数、类型、trait、impl、const、static、mod、use、extern。

use crate::error::CompileError;
use crate::lexer::TokenKind;
use crate::parser::ast::*;
use crate::parser::Parser;

impl Parser {
    // ==================================================================
    // use
    // ==================================================================

    pub fn parse_use(&mut self, _attrs: Vec<Attribute>) -> Result<UseDecl, CompileError> {
        let span = self.previous().span;
        let tree = self.parse_use_tree()?;
        self.consume(&TokenKind::Semicolon, "expected ';' after use declaration")?;
        Ok(UseDecl { tree, span })
    }

    fn parse_use_tree(&mut self) -> Result<UseTree, CompileError> {
        // 处理 `use a::b as c`
        if self.check(&TokenKind::KwSelf)
            || matches!(self.peek().kind, TokenKind::Ident(_))
            || self.check(&TokenKind::Star)
        {
            let mut path = Vec::new();
            if self.match_kind(&TokenKind::KwSelf) {
                path.push("self".into());
            } else {
                path.push(self.consume_ident("expected name in use tree")?);
            }
            while self.match_kind(&TokenKind::ColonColon) {
                if self.match_kind(&TokenKind::Star) {
                    return Ok(UseTree::Group(vec![UseTree::Glob]));
                }
                path.push(self.consume_ident("expected name")?);
            }
            // 构造嵌套 Name 树
            let mut tree = UseTree::Name(path[0].clone());
            for seg in path.into_iter().skip(1) {
                tree = UseTree::Group(vec![tree, UseTree::Name(seg)]);
            }
            // `as` 重命名
            if self.match_kind(&TokenKind::KwAs) {
                let alias = self.consume_ident("expected alias name")?;
                return Ok(UseTree::Rename(Box::new(tree), alias));
            }
            return Ok(tree);
        }

        if self.match_kind(&TokenKind::Star) {
            return Ok(UseTree::Glob);
        }

        Err(self.unexpected(&["identifier", "self", "*"]))
    }

    // ==================================================================
    // mod
    // ==================================================================

    pub fn parse_mod(
        &mut self,
        visibility: Visibility,
        _attrs: Vec<Attribute>,
    ) -> Result<ModuleDecl, CompileError> {
        let span = self.previous().span;
        let name = self.consume_ident("expected module name")?;
        if self.match_kind(&TokenKind::Semicolon) {
            Ok(ModuleDecl::Load { name, visibility, span })
        } else if self.match_kind(&TokenKind::LBrace) {
            let mut inner = Program::default();
            // 递归解析模块体（简化：复用顶层解析逻辑）
            while !self.check(&TokenKind::RBrace) && !self.is_at_end() {
                let item = self.parse_module_item()?;
                match item {
                    ModuleItem::Function(f) => inner.functions.push(f),
                    ModuleItem::TypeDef(t) => inner.type_defs.push(t),
                    ModuleItem::Trait(t) => inner.traits.push(t),
                    ModuleItem::Impl(i) => inner.impls.push(i),
                    ModuleItem::Const(c) => inner.consts.push(c),
                    ModuleItem::Static(s) => inner.statics.push(s),
                    ModuleItem::Use(u) => inner.imports.push(u),
                    ModuleItem::Module(m) => inner.modules.push(m),
                    ModuleItem::Extern(e) => inner.externs.push(e),
                    ModuleItem::Attribute(a) => inner.attrs.push(a),
                }
            }
            self.consume(&TokenKind::RBrace, "expected '}' to close module")?;
            Ok(ModuleDecl::Inline { name, visibility, body: inner, span })
        } else {
            Err(self.expected("expected ';' or '{' after module name", &TokenKind::LBrace))
        }
    }

    // ==================================================================
    // extern
    // ==================================================================

    pub fn parse_extern(
        &mut self,
        _attrs: Vec<Attribute>,
    ) -> Result<ExternBlock, CompileError> {
        let span = self.previous().span;
        let abi = if self.match_kind(&TokenKind::StringLiteral) {
            // 上一个 token 已被消费，abi 字符串需要从 previous() 获取
            match &self.previous().kind {
                TokenKind::StringLiteral(s) => match s.as_str() {
                    "C" => Abi::C,
                    "cdecl" => Abi::Cdecl,
                    "stdcall" => Abi::Stdcall,
                    "fastcall" => Abi::Fastcall,
                    "system" => Abi::System,
                    "zr" => Abi::Zr,
                    other => Abi::Other(other.to_string()),
                },
                _ => Abi::C,
            }
        } else {
            Abi::C
        };
        self.consume(&TokenKind::LBrace, "expected '{' after extern")?;
        let mut items = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.is_at_end() {
            if self.match_kind(&TokenKind::KwFn) {
                items.push(ExternItem::Function(self.parse_function(Visibility::Private, vec![])?));
            } else if self.match_kind(&TokenKind::KwType) {
                let name = self.consume_ident("expected type name")?;
                self.consume(&TokenKind::Semicolon, "expected ';'")?;
                items.push(ExternItem::Type(name, Span::unknown()));
            } else {
                return Err(self.unexpected(&["fn", "type"]));
            }
        }
        self.consume(&TokenKind::RBrace, "expected '}'")?;
        Ok(ExternBlock { abi, items, span })
    }

    // ==================================================================
    // fn
    // ==================================================================

    pub fn parse_function(
        &mut self,
        visibility: Visibility,
        attrs: Vec<Attribute>,
    ) -> Result<Function, CompileError> {
        let span = self.previous().span;
        let is_async = self.match_kind(&TokenKind::KwAsync);
        let is_const = self.match_kind(&TokenKind::KwComptime);
        let name = self.consume_ident("expected function name")?;

        // 泛型参数
        let generics = if self.match_kind(&TokenKind::Lt) {
            self.parse_generic_params()?
        } else {
            vec![]
        };

        self.consume(&TokenKind::LParen, "expected '(' after function name")?;

        // `self` / `&self` / `&mut self` 接收者
        let mut receiver: Option<Receiver> = None;
        let mut params: Vec<Param> = Vec::new();

        if !self.check(&TokenKind::RParen) {
            // 检查接收者
            if self.check(&TokenKind::KwSelf) || self.check(&TokenKind::Amp) {
                receiver = Some(self.parse_receiver()?);
                if !self.check(&TokenKind::RParen) {
                    self.consume(&TokenKind::Comma, "expected ',' or ')' after receiver")?;
                }
            }

            if !self.check(&TokenKind::RParen) {
                params.push(self.parse_param()?);
                while self.match_kind(&TokenKind::Comma) {
                    if self.check(&TokenKind::RParen) {
                        break;
                    }
                    params.push(self.parse_param()?);
                }
            }
        }
        self.consume(&TokenKind::RParen, "expected ')'")?;

        // 返回类型
        let return_type = if self.match_kind(&TokenKind::Arrow) {
            self.parse_type()?
        } else {
            Type::Void
        };

        // `where` 子句
        let where_clause = if self.match_kind(&TokenKind::KwWhere) {
            self.parse_where_clause()?
        } else {
            vec![]
        };

        // 函数体（可省略 = 声明 / 外部函数）
        let body = if self.check(&TokenKind::Semicolon) {
            self.advance();
            None
        } else {
            Some(self.parse_block()?)
        };

        let is_noreturn = matches!(return_type, Type::Never);

        Ok(Function {
            name,
            visibility,
            attrs,
            is_async,
            is_noreturn,
            is_const,
            abi: Abi::Zr,
            generics,
            where_clause,
            params,
            receiver,
            return_type,
            body,
            span,
        })
    }

    fn parse_param(&mut self) -> Result<Param, CompileError> {
        // 支持 `name: type` 与解构 `Pattern: type`
        let pattern = self.parse_pattern()?;
        self.consume(&TokenKind::Colon, "expected ':' after parameter name")?;
        let ty = self.parse_type()?;
        Ok(Param { pattern, ty, span: Span::unknown() })
    }

    fn parse_receiver(&mut self) -> Result<Receiver, CompileError> {
        let span = self.peek().span;
        let mutability = if self.match_kind(&TokenKind::Amp) {
            if self.match_kind(&TokenKind::KwMut) {
                crate::parser::ast::Mutability::Mutable
            } else {
                crate::parser::ast::Mutability::Immutable
            }
        } else {
            crate::parser::ast::Mutability::Immutable
        };
        self.consume(&TokenKind::KwSelf, "expected 'self'")?;
        Ok(Receiver { mutability, ty: None, span })
    }

    fn parse_where_clause(&mut self) -> Result<Vec<WherePred>, CompileError> {
        let mut preds = Vec::new();
        preds.push(self.parse_where_pred()?);
        while self.match_kind(&TokenKind::Comma) {
            if self.check(&TokenKind::LBrace) || self.check(&TokenKind::Arrow) {
                break;
            }
            preds.push(self.parse_where_pred()?);
        }
        Ok(preds)
    }

    fn parse_where_pred(&mut self) -> Result<WherePred, CompileError> {
        let span = self.peek().span;
        let lhs = self.parse_type()?;
        self.consume(&TokenKind::Colon, "expected ':' in where predicate")?;
        let trait_type = self.parse_type()?;
        Ok(WherePred { lhs, trait_type, span })
    }

    // ==================================================================
    // const / static
    // ==================================================================

    pub fn parse_const(
        &mut self,
        visibility: Visibility,
        _attrs: Vec<Attribute>,
    ) -> Result<ConstDecl, CompileError> {
        let span = self.previous().span;
        let name = self.consume_ident("expected constant name")?;
        self.consume(&TokenKind::Colon, "expected ':'")?;
        let ty = self.parse_type()?;
        self.consume(&TokenKind::Eq, "expected '='")?;
        let value = self.parse_expression()?;
        self.consume(&TokenKind::Semicolon, "expected ';'")?;
        Ok(ConstDecl { name, visibility, ty, value, span })
    }

    pub fn parse_static(
        &mut self,
        visibility: Visibility,
        _attrs: Vec<Attribute>,
    ) -> Result<StaticDecl, CompileError> {
        let span = self.previous().span;
        let mutable = self.match_kind(&TokenKind::KwMut);
        let name = self.consume_ident("expected static name")?;
        self.consume(&TokenKind::Colon, "expected ':'")?;
        let ty = self.parse_type()?;
        self.consume(&TokenKind::Eq, "expected '='")?;
        let init = self.parse_expression()?;
        self.consume(&TokenKind::Semicolon, "expected ';'")?;
        Ok(StaticDecl { name, visibility, mutable, ty, init, span })
    }

    // ==================================================================
    // struct
    // ==================================================================

    pub fn parse_struct_def(
        &mut self,
        visibility: Visibility,
        attrs: Vec<Attribute>,
    ) -> Result<TypeDef, CompileError> {
        let span = self.previous().span;
        let name = self.consume_ident("expected struct name")?;
        let generics = if self.match_kind(&TokenKind::Lt) {
            self.parse_generic_params()?
        } else {
            vec![]
        };

        if self.match_kind(&TokenKind::LParen) {
            // 元组结构体 `struct Point(u32, u32)`
            let mut fields = Vec::new();
            if !self.check(&TokenKind::RParen) {
                fields.push(Field {
                    name: String::new(),
                    ty: self.parse_type()?,
                    visibility: Visibility::Private,
                    default: None,
                    span: Span::unknown(),
                });
                while self.match_kind(&TokenKind::Comma) {
                    if self.check(&TokenKind::RParen) {
                        break;
                    }
                    fields.push(Field {
                        name: String::new(),
                        ty: self.parse_type()?,
                        visibility: Visibility::Private,
                        default: None,
                        span: Span::unknown(),
                    });
                }
            }
            self.consume(&TokenKind::RParen, "expected ')'")?;
            self.consume(&TokenKind::Semicolon, "expected ';' after tuple struct")?;
            return Ok(TypeDef::Struct(StructDef {
                name,
                visibility,
                attrs,
                generics,
                fields,
                tuple_like: true,
                span,
            }));
        }

        self.consume(&TokenKind::LBrace, "expected '{' after struct name")?;
        let mut fields = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.is_at_end() {
            let fvis = self.parse_visibility();
            let fname = self.consume_ident("expected field name")?;
            self.consume(&TokenKind::Colon, "expected ':' after field name")?;
            let fty = self.parse_type()?;
            let default = if self.match_kind(&TokenKind::Eq) {
                Some(self.parse_expression()?)
            } else {
                None
            };
            fields.push(Field {
                name: fname,
                ty: fty,
                visibility: fvis,
                default,
                span: Span::unknown(),
            });
            self.consume(&TokenKind::Comma, "expected ',' after field")?;
        }
        self.consume(&TokenKind::RBrace, "expected '}' to close struct")?;
        Ok(TypeDef::Struct(StructDef {
            name,
            visibility,
            attrs,
            generics,
            fields,
            tuple_like: false,
            span,
        }))
    }

    // ==================================================================
    // enum
    // ==================================================================

    pub fn parse_enum_def(
        &mut self,
        visibility: Visibility,
        attrs: Vec<Attribute>,
    ) -> Result<TypeDef, CompileError> {
        let span = self.previous().span;
        let name = self.consume_ident("expected enum name")?;
        let generics = if self.match_kind(&TokenKind::Lt) {
            self.parse_generic_params()?
        } else {
            vec![]
        };
        // `repr` 属性
        let repr = if self.match_kind(&TokenKind::Colon) {
            Some(self.parse_type()?)
        } else {
            None
        };
        self.consume(&TokenKind::LBrace, "expected '{' after enum name")?;
        let mut variants = Vec::new();
        if !self.check(&TokenKind::RBrace) {
            variants.push(self.parse_variant()?);
            while self.match_kind(&TokenKind::Comma) {
                if self.check(&TokenKind::RBrace) {
                    break;
                }
                variants.push(self.parse_variant()?);
            }
        }
        self.consume(&TokenKind::RBrace, "expected '}' to close enum")?;
        Ok(TypeDef::Enum(EnumDef { name, visibility, generics, repr, variants, span }))
    }

    fn parse_variant(&mut self) -> Result<Variant, CompileError> {
        let span = self.peek().span;
        let name = self.consume_ident("expected variant name")?;
        let payload = if self.match_kind(&TokenKind::LParen) {
            let mut types = Vec::new();
            if !self.check(&TokenKind::RParen) {
                types.push(self.parse_type()?);
                while self.match_kind(&TokenKind::Comma) {
                    if self.check(&TokenKind::RParen) {
                        break;
                    }
                    types.push(self.parse_type()?);
                }
            }
            self.consume(&TokenKind::RParen, "expected ')'")?;
            if types.is_empty() {
                VariantPayload::Unit
            } else {
                VariantPayload::Tuple(types)
            }
        } else if self.match_kind(&TokenKind::LBrace) {
            let mut fields = Vec::new();
            while !self.check(&TokenKind::RBrace) && !self.is_at_end() {
                let fname = self.consume_ident("expected field name")?;
                self.consume(&TokenKind::Colon, "expected ':'")?;
                let fty = self.parse_type()?;
                fields.push(Field {
                    name: fname,
                    ty: fty,
                    visibility: Visibility::Private,
                    default: None,
                    span: Span::unknown(),
                });
                self.consume(&TokenKind::Comma, "expected ',' after field")?;
            }
            self.consume(&TokenKind::RBrace, "expected '}'")?;
            VariantPayload::Struct(fields)
        } else {
            VariantPayload::Unit
        };
        let discriminant = if self.match_kind(&TokenKind::Eq) {
            Some(self.parse_expression()?)
        } else {
            None
        };
        Ok(Variant::Variant { name, payload, discriminant, span })
    }

    // ==================================================================
    // union
    // ==================================================================

    pub fn parse_union_def(
        &mut self,
        visibility: Visibility,
        attrs: Vec<Attribute>,
    ) -> Result<TypeDef, CompileError> {
        let span = self.previous().span;
        let name = self.consume_ident("expected union name")?;
        let generics = if self.match_kind(&TokenKind::Lt) {
            self.parse_generic_params()?
        } else {
            vec![]
        };
        self.consume(&TokenKind::LBrace, "expected '{'")?;
        let mut fields = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.is_at_end() {
            let fvis = self.parse_visibility();
            let fname = self.consume_ident("expected field name")?;
            self.consume(&TokenKind::Colon, "expected ':'")?;
            let fty = self.parse_type()?;
            fields.push(Field {
                name: fname,
                ty: fty,
                visibility: fvis,
                default: None,
                span: Span::unknown(),
            });
            self.consume(&TokenKind::Comma, "expected ',' after field")?;
        }
        self.consume(&TokenKind::RBrace, "expected '}'")?;
        Ok(TypeDef::Union(UnionDef { name, visibility, generics, fields, span }))
    }

    // ==================================================================
    // type alias
    // ==================================================================

    pub fn parse_type_alias(
        &mut self,
        visibility: Visibility,
        _attrs: Vec<Attribute>,
    ) -> Result<TypeDef, CompileError> {
        let span = self.previous().span;
        let name = self.consume_ident("expected type name")?;
        let generics = if self.match_kind(&TokenKind::Lt) {
            self.parse_generic_params()?
        } else {
            vec![]
        };
        self.consume(&TokenKind::Eq, "expected '=' in type alias")?;
        let target = self.parse_type()?;
        self.consume(&TokenKind::Semicolon, "expected ';'")?;
        Ok(TypeDef::Alias { name, generics, target, visibility, span })
    }

    // ==================================================================
    // trait
    // ==================================================================

    pub fn parse_trait_def(
        &mut self,
        visibility: Visibility,
        _attrs: Vec<Attribute>,
    ) -> Result<TraitDef, CompileError> {
        let span = self.previous().span;
        let name = self.consume_ident("expected trait name")?;
        let generics = if self.match_kind(&TokenKind::Lt) {
            self.parse_generic_params()?
        } else {
            vec![]
        };
        let super_traits = if self.match_kind(&TokenKind::Colon) {
            let mut st = vec![self.parse_type()?];
            while self.match_kind(&TokenKind::Plus) {
                st.push(self.parse_type()?);
            }
            st
        } else {
            vec![]
        };
        self.consume(&TokenKind::LBrace, "expected '{'")?;
        let mut assoc_types = Vec::new();
        let mut methods = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.is_at_end() {
            if self.match_kind(&TokenKind::KwType) {
                assoc_types.push(self.parse_assoc_type()?);
            } else {
                methods.push(self.parse_function(Visibility::Private, vec![])?);
            }
        }
        self.consume(&TokenKind::RBrace, "expected '}'")?;
        Ok(TraitDef { name, visibility, generics, super_traits, assoc_types, methods, span })
    }

    fn parse_assoc_type(&mut self) -> Result<AssocType, CompileError> {
        let span = self.peek().span;
        let name = self.consume_ident("expected associated type name")?;
        let mut bounds = Vec::new();
        if self.match_kind(&TokenKind::Colon) {
            bounds.push(self.parse_type()?);
            while self.match_kind(&TokenKind::Plus) {
                bounds.push(self.parse_type()?);
            }
        }
        let default = if self.match_kind(&TokenKind::Eq) {
            Some(self.parse_type()?);
        } else {
            None
        };
        self.consume(&TokenKind::Semicolon, "expected ';'")?;
        Ok(AssocType { name, bounds, default, span })
    }

    // ==================================================================
    // impl
    // ==================================================================

    pub fn parse_impl_block(
        &mut self,
        _attrs: Vec<Attribute>,
    ) -> Result<ImplBlock, CompileError> {
        let span = self.previous().span;
        let generics = if self.match_kind(&TokenKind::Lt) {
            self.parse_generic_params()?
        } else {
            vec![]
        };
        let target = self.parse_type()?;
        let trait_type = if self.match_kind(&TokenKind::KwFor) {
            Some(target)
        } else {
            None
        };
        let target = if trait_type.is_some() {
            self.parse_type()?
        } else {
            target
        };
        let where_clause = if self.match_kind(&TokenKind::KwWhere) {
            self.parse_where_clause()?
        } else {
            vec![]
        };
        self.consume(&TokenKind::LBrace, "expected '{'")?;
        let mut items = Vec::new();
        let mut assoc_types = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.is_at_end() {
            if self.match_kind(&TokenKind::KwType) {
                assoc_types.push(self.parse_assoc_type()?);
            } else {
                items.push(self.parse_function(Visibility::Private, vec![])?);
            }
        }
        self.consume(&TokenKind::RBrace, "expected '}'")?;
        Ok(ImplBlock { generics, target, trait_type, where_clause, items, assoc_types, span })
    }

    // ==================================================================
    // 泛型参数
    // ==================================================================

    fn parse_generic_params(&mut self) -> Result<Vec<GenericParam>, CompileError> {
        let mut params = Vec::new();
        if !self.check(&TokenKind::Gt) {
            params.push(self.parse_generic_param()?);
            while self.match_kind(&TokenKind::Comma) {
                if self.check(&TokenKind::Gt) {
                    break;
                }
                params.push(self.parse_generic_param()?);
            }
        }
        self.consume(&TokenKind::Gt, "expected '>' to close generic parameters")?;
        Ok(params)
    }

    fn parse_generic_param(&mut self) -> Result<GenericParam, CompileError> {
        if self.match_kind(&TokenKind::KwConst) {
            let name = self.consume_ident("expected const parameter name")?;
            self.consume(&TokenKind::Colon, "expected ':'")?;
            let ty = self.parse_type()?;
            let default = if self.match_kind(&TokenKind::Eq) {
                Some(self.parse_expression()?)
            } else {
                None
            };
            Ok(GenericParam::Const { name, ty, default })
        } else {
            let name = self.consume_ident("expected generic parameter name")?;
            let mut bounds = Vec::new();
            if self.match_kind(&TokenKind::Colon) {
                bounds.push(self.parse_type()?);
                while self.match_kind(&TokenKind::Plus) {
                    bounds.push(self.parse_type()?);
                }
            }
            let default = if self.match_kind(&TokenKind::Eq) {
                Some(self.parse_type()?)
            } else {
                None
            };
            Ok(GenericParam::Type { name, bounds, default })
        }
    }
}
