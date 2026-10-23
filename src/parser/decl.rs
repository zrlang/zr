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
        // 路径段可以是标识符，也可以是关键字（模块名 `async` 等）。
        if self.check(&TokenKind::KwSelf)
            || matches!(self.peek().kind, TokenKind::Ident(_))
            || self.peek().kind.keyword_text().is_some()
            || self.check(&TokenKind::Star)
        {
            let mut path = Vec::new();
            if self.match_kind(&TokenKind::KwSelf) {
                path.push("self".into());
            } else {
                path.push(self.consume_path_segment("expected name in use tree")?);
            }
            while self.match_kind(&TokenKind::ColonColon) {
                if self.match_kind(&TokenKind::Star) {
                    return Ok(UseTree::Group(vec![UseTree::Glob]));
                }
                if self.match_kind(&TokenKind::LBrace) {
                    let mut trees = Vec::new();
                    if !self.check(&TokenKind::RBrace) {
                        trees.push(self.parse_use_tree()?);
                        while self.match_kind(&TokenKind::Comma) {
                            if self.check(&TokenKind::RBrace) {
                                break;
                            }
                            trees.push(self.parse_use_tree()?);
                        }
                    }
                    self.consume(&TokenKind::RBrace, "expected '}' to close use group")?;
                    let group = UseTree::Group(trees);
                    // 把已解析的 path 包装到 group 外面
                    let mut tree = UseTree::Name(path[0].clone());
                    for seg in path.into_iter().skip(1) {
                        tree = UseTree::Group(vec![tree, UseTree::Name(seg)]);
                    }
                    return Ok(UseTree::Group(vec![tree, group]));
                }
                path.push(self.consume_path_segment("expected name")?);
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
    // macro_rules!
    // ==================================================================

    pub fn parse_macro_rules(&mut self, _attrs: Vec<Attribute>) -> Result<MacroDef, CompileError> {
        let span = self.previous().span;
        let name = self.consume_ident("expected macro name")?;
        self.consume(&TokenKind::LBrace, "expected '{' after macro name")?;

        let mut arms: Vec<MacroArm> = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.is_at_end() {
            // 左侧：匹配模式，收集到 `=>` 为止（保留括号等结构）。
            let mut pattern: Vec<TokenKind> = Vec::new();
            while !self.check(&TokenKind::Rocket) && !self.is_at_end() {
                pattern.push(self.advance().kind);
            }
            self.consume(&TokenKind::Rocket, "expected '=>' in macro arm")?;

            // 右侧：展开体，收集到臂结束为止。
            //
            // 结束条件：顶层遇到 `;` 或 `}`。括号内的 `;`/`}` 属于
            // 嵌套结构，要按深度计数，不能提前收尾。
            let mut body: Vec<TokenKind> = Vec::new();
            let mut depth: i32 = 0;
            loop {
                if self.is_at_end() {
                    break;
                }
                let kind = self.peek().kind.clone();
                match &kind {
                    TokenKind::LParen | TokenKind::LBrace | TokenKind::LBracket => {
                        depth += 1;
                    }
                    TokenKind::RParen | TokenKind::RBrace | TokenKind::RBracket => {
                        if depth == 0 {
                            // 顶层 `}`：宏体结束（外层大括号）。
                            break;
                        }
                        depth -= 1;
                    }
                    TokenKind::Semicolon if depth == 0 => {
                        // 顶层 `;`：一条臂结束。
                        self.advance();
                        break;
                    }
                    _ => {}
                }
                body.push(self.advance().kind);
            }
            arms.push(MacroArm { pattern, body });
        }
        self.consume(&TokenKind::RBrace, "expected '}' to close macro")?;

        Ok(MacroDef {
            name,
            arms,
            span,
        })
    }

    // ==================================================================
    // mod
    // ==================================================================

    pub fn parse_mod(
        &mut self,
        _visibility: Visibility,
        _attrs: Vec<Attribute>,
    ) -> Result<ModuleDecl, CompileError> {
        let _span = self.previous().span;
        let name = self.consume_ident("expected module name")?;
        if self.match_kind(&TokenKind::Semicolon) {
            Ok(ModuleDecl::Load(name))
        } else if self.match_kind(&TokenKind::LBrace) {
            // 递归解析模块体，条目原样保留：模块系统是扁平命名空间，
            // 内联模块里的 `fn`/`struct` 会被直接合并进顶层。
            let mut items = Vec::new();
            while !self.check(&TokenKind::RBrace) && !self.is_at_end() {
                items.push(self.parse_module_item()?);
            }
            self.consume(&TokenKind::RBrace, "expected '}' to close module")?;
            Ok(ModuleDecl::Inline(name, items))
        } else {
            Err(self.expected("expected ';' or '{' after module name", &TokenKind::LBrace))
        }
    }

    // ==================================================================
    // extern
    // ==================================================================

    pub fn parse_extern(&mut self, _attrs: Vec<Attribute>) -> Result<ExternBlock, CompileError> {
        let _span = self.previous().span;
        // `extern "C" { ... }`：ABI 名字串被消费（目前只支持 C ABI）。
        if matches!(self.peek().kind, TokenKind::StringLiteral(_)) {
            self.advance();
        }
        self.consume(&TokenKind::LBrace, "expected '{' after extern")?;
        let mut functions = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.is_at_end() {
            if self.match_kind(&TokenKind::KwFn) {
                // 外部函数的签名必须保留——早先这里把解析结果直接丢掉，
                // 于是 `extern "C" { fn zr_write(...); }` 之后调用 `zr_write`
                // 会报"未定义名称"。解析出的 `Function` 的 `body` 为 `None`
                // （以 `;` 结尾）。
                functions.push(self.parse_function(Visibility::Private, vec![], false, false)?);
            } else if self.match_kind(&TokenKind::KwType) {
                let _name = self.consume_ident("expected type name")?;
                self.consume(&TokenKind::Semicolon, "expected ';'")?;
            } else {
                return Err(self.unexpected(&["fn", "type"]));
            }
        }
        self.consume(&TokenKind::RBrace, "expected '}'")?;
        Ok(ExternBlock { functions })
    }

    // ==================================================================
    // fn
    // ==================================================================

    pub fn parse_function(
        &mut self,
        _visibility: Visibility,
        attrs: Vec<Attribute>,
        is_async: bool,
        _is_default: bool,
    ) -> Result<Function, CompileError> {
        self.parse_function_ext(_visibility, attrs, is_async, _is_default, false)
    }

    /// `parse_function` 的完整版本，多带一个 `comptime` 标志。
    ///
    /// `comptime fn` 的修饰符由条目分流层吃掉（和 `async fn` / `default fn`
    /// 一样，两个修饰符共用一条解析路径），所以标志得由调用方传进来。
    pub fn parse_function_ext(
        &mut self,
        _visibility: Visibility,
        attrs: Vec<Attribute>,
        is_async: bool,
        _is_default: bool,
        is_comptime: bool,
    ) -> Result<Function, CompileError> {
        let _span = self.previous().span;
        // `comptime fn` 也可以在分流层没识别的情况下走到这里（例如 impl 块里），
        // 所以这里仍然兜底吃一次 `comptime` 修饰符。
        let is_comptime = is_comptime || self.match_kind(&TokenKind::KwComptime);
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
                    // 可变参数 `...`
                    if self.check(&TokenKind::DotDot)
                        && self
                            .peek_n(1)
                            .map(|t| t.kind == TokenKind::Dot)
                            .unwrap_or(false)
                    {
                        self.advance(); // ..
                        self.advance(); // .
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

        // `where` 子句（解析以消费 token，但不存储）
        if self.match_kind(&TokenKind::KwWhere) {
            let _ = self.parse_where_clause()?;
        }

        // 函数体（可省略 = 声明 / 外部函数）
        let body = if self.check(&TokenKind::Semicolon) {
            self.advance();
            None
        } else {
            Some(self.parse_block()?)
        };

        let _is_noreturn = matches!(return_type, Type::Never);

        Ok(Function {
            name,
            generics,
            // 属性之前被直接丢掉（`_attrs`），于是 `#[inline]`、`#[test]`
            // 之类只能解析、完全没有语义。必须原样带到 AST 上去。
            attrs,
            is_async,
            is_comptime,
            params,
            receiver,
            return_type,
            body,
        })
    }

    fn parse_param(&mut self) -> Result<Param, CompileError> {
        // 支持 `name: type` 与解构 `Pattern: type`
        let pattern = self.parse_pattern()?;
        self.consume(&TokenKind::Colon, "expected ':' after parameter name")?;
        let ty = self.parse_type()?;
        Ok(Param { pattern, ty })
    }

    fn parse_receiver(&mut self) -> Result<Receiver, CompileError> {
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
        // 显式接收者类型：`self: &mut Self`
        let ty = if self.match_kind(&TokenKind::Colon) {
            Some(self.parse_type()?)
        } else {
            None
        };
        Ok(Receiver { mutability, ty })
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
        let _span = self.peek().span;
        // 支持 `for<'a> T: Trait` 高阶 trait 边界
        let _for_lifetimes = if self.match_kind(&TokenKind::KwFor) {
            self.consume(&TokenKind::Lt, "expected '<' after 'for'")?;
            let mut lts = Vec::new();
            if !self.check(&TokenKind::Gt) {
                lts.push(self.consume_ident("expected lifetime name")?);
                while self.match_kind(&TokenKind::Comma) {
                    if self.check(&TokenKind::Gt) {
                        break;
                    }
                    lts.push(self.consume_ident("expected lifetime name")?);
                }
            }
            self.consume(&TokenKind::Gt, "expected '>' to close lifetime parameters")?;
            lts
        } else {
            Vec::new()
        };
        let _lhs = self.parse_type()?;
        self.consume(&TokenKind::Colon, "expected ':' in where predicate")?;
        self.parse_where_bound()?;
        // 多边界 `T: Copy + Clone`
        while self.match_kind(&TokenKind::Plus) {
            self.parse_where_bound()?;
        }
        Ok(WherePred)
    }

    /// 解析单个 trait / 生命周期边界。
    ///
    /// `'a` / `'static` 不是类型，`parse_type` 会把它当"生命周期前缀"跳过
    /// 然后去解析本不存在的类型（`where T: 'static {` 报 `expected type,
    /// found LBrace`）。这里单独消费。
    fn parse_where_bound(&mut self) -> Result<(), CompileError> {
        let _negated = self.match_kind(&TokenKind::Bang);
        if let TokenKind::Lifetime(_) = self.peek().kind {
            self.advance();
            return Ok(());
        }
        let _ = self.parse_type()?;
        Ok(())
    }

    // ==================================================================
    // const / static
    // ==================================================================

    pub fn parse_const(
        &mut self,
        _visibility: Visibility,
        _attrs: Vec<Attribute>,
    ) -> Result<ConstDecl, CompileError> {
        let _span = self.previous().span;
        let name = self.consume_ident("expected constant name")?;
        self.consume(&TokenKind::Colon, "expected ':'")?;
        let ty = self.parse_type()?;
        self.consume(&TokenKind::Eq, "expected '='")?;
        let value = self.parse_expression()?;
        self.consume(&TokenKind::Semicolon, "expected ';'")?;
        Ok(ConstDecl { name, ty, value })
    }

    pub fn parse_static(
        &mut self,
        _visibility: Visibility,
        _attrs: Vec<Attribute>,
    ) -> Result<StaticDecl, CompileError> {
        let _span = self.previous().span;
        let is_mut = self.match_kind(&TokenKind::KwMut);
        let name = self.consume_ident("expected static name")?;
        self.consume(&TokenKind::Colon, "expected ':'")?;
        let ty = self.parse_type()?;
        self.consume(&TokenKind::Eq, "expected '='")?;
        let init = self.parse_expression()?;
        self.consume(&TokenKind::Semicolon, "expected ';'")?;
        Ok(StaticDecl {
            name,
            is_mut,
            ty,
            init,
        })
    }

    // ==================================================================
    // struct
    // ==================================================================

    pub fn parse_struct_def(
        &mut self,
        _visibility: Visibility,
        attrs: Vec<Attribute>,
    ) -> Result<TypeDef, CompileError> {
        let _span = self.previous().span;
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
                });
                while self.match_kind(&TokenKind::Comma) {
                    if self.check(&TokenKind::RParen) {
                        break;
                    }
                    fields.push(Field {
                        name: String::new(),
                        ty: self.parse_type()?,
                    });
                }
            }
            self.consume(&TokenKind::RParen, "expected ')'")?;
            self.consume(&TokenKind::Semicolon, "expected ';' after tuple struct")?;
            return Ok(TypeDef::Struct(StructDef {
                name,
                generics,
                attrs,
                fields,
            }));
        }

        // 单元结构体 `struct SystemAllocator;`——没有字段，也没有大括号。
        if self.match_kind(&TokenKind::Semicolon) {
            return Ok(TypeDef::Struct(StructDef {
                name,
                generics,
                attrs,
                fields: Vec::new(),
            }));
        }

        self.consume(&TokenKind::LBrace, "expected '{' after struct name")?;
        let mut fields = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.is_at_end() {
            let _fvis = self.parse_visibility();
            let fname = self.consume_ident("expected field name")?;
            self.consume(&TokenKind::Colon, "expected ':' after field name")?;
            let fty = self.parse_type()?;
            if self.match_kind(&TokenKind::Eq) {
                // 字段默认值已被消费，丢弃
                let _ = self.parse_expression()?;
            }
            fields.push(Field {
                name: fname,
                ty: fty,
            });
            // 最后一个字段后面允许省略逗号
            if !self.match_kind(&TokenKind::Comma) {
                break;
            }
        }
        self.consume(&TokenKind::RBrace, "expected '}' to close struct")?;
        Ok(TypeDef::Struct(StructDef {
            name,
            generics,
            attrs,
            fields,
        }))
    }

    // ==================================================================
    // enum
    // ==================================================================

    pub fn parse_enum_def(
        &mut self,
        _visibility: Visibility,
        attrs: Vec<Attribute>,
    ) -> Result<TypeDef, CompileError> {
        let _span = self.previous().span;
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
        Ok(TypeDef::Enum(EnumDef {
            name,
            attrs,
            generics,
            repr,
            variants,
        }))
    }

    fn parse_variant(&mut self) -> Result<Variant, CompileError> {
        let _span = self.peek().span;
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
                });
                // 最后一个字段后面允许省略逗号。
                if !self.match_kind(&TokenKind::Comma) {
                    break;
                }
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
        Ok(Variant {
            name,
            payload,
            discriminant,
        })
    }

    // ==================================================================
    // union
    // ==================================================================

    pub fn parse_union_def(
        &mut self,
        _visibility: Visibility,
        _attrs: Vec<Attribute>,
    ) -> Result<TypeDef, CompileError> {
        let _span = self.previous().span;
        let name = self.consume_ident("expected union name")?;
        let _generics = if self.match_kind(&TokenKind::Lt) {
            self.parse_generic_params()?
        } else {
            vec![]
        };
        self.consume(&TokenKind::LBrace, "expected '{'")?;
        let mut fields = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.is_at_end() {
            let _fvis = self.parse_visibility();
            let fname = self.consume_ident("expected field name")?;
            self.consume(&TokenKind::Colon, "expected ':'")?;
            let fty = self.parse_type()?;
            fields.push(Field {
                name: fname,
                ty: fty,
            });
            // 最后一个字段后面允许省略逗号
            if !self.match_kind(&TokenKind::Comma) {
                break;
            }
        }
        self.consume(&TokenKind::RBrace, "expected '}'")?;
        Ok(TypeDef::Union(UnionDef { name, fields }))
    }

    // ==================================================================
    // type alias
    // ==================================================================

    pub fn parse_type_alias(
        &mut self,
        _visibility: Visibility,
        _attrs: Vec<Attribute>,
    ) -> Result<TypeDef, CompileError> {
        let _span = self.previous().span;
        let name = self.consume_ident("expected type name")?;
        let _generics = if self.match_kind(&TokenKind::Lt) {
            self.parse_generic_params()?
        } else {
            vec![]
        };
        self.consume(&TokenKind::Eq, "expected '=' in type alias")?;
        let target = self.parse_type()?;
        self.consume(&TokenKind::Semicolon, "expected ';'")?;
        Ok(TypeDef::Alias { name, target })
    }

    // ==================================================================
    // trait
    // ==================================================================

    pub fn parse_trait_def(
        &mut self,
        _visibility: Visibility,
        attrs: Vec<Attribute>,
    ) -> Result<TraitDef, CompileError> {
        let _span = self.previous().span;
        let name = self.consume_ident("expected trait name")?;
        let _generics = if self.match_kind(&TokenKind::Lt) {
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
                // impl 里的方法可以带可见性修饰（`pub fn new`）
                let _pub = self.match_kind(&TokenKind::KwPub);
                let is_async = self.match_kind(&TokenKind::KwAsync);
                self.consume(&TokenKind::KwFn, "expected 'fn'")?;
                methods.push(self.parse_function(Visibility::Private, vec![], is_async, false)?);
            }
        }
        self.consume(&TokenKind::RBrace, "expected '}'")?;
        Ok(TraitDef {
            name,
            super_traits,
            assoc_types,
            methods,
            attrs,
        })
    }

    fn parse_assoc_type(&mut self) -> Result<AssocType, CompileError> {
        let _span = self.peek().span;
        let _name = self.consume_ident("expected associated type name")?;
        if self.match_kind(&TokenKind::Colon) {
            // 消费 trait bounds 但不存储
            let _ = self.parse_type()?;
            while self.match_kind(&TokenKind::Plus) {
                let _ = self.parse_type()?;
            }
        }
        let _default = if self.match_kind(&TokenKind::Eq) {
            Some(self.parse_type()?)
        } else {
            None
        };
        self.consume(&TokenKind::Semicolon, "expected ';'")?;
        Ok(AssocType)
    }

    // ==================================================================
    // impl
    // ==================================================================

    pub fn parse_impl_block(&mut self, _attrs: Vec<Attribute>) -> Result<ImplBlock, CompileError> {
        let span = self.previous().span;
        let generics = if self.match_kind(&TokenKind::Lt) {
            self.parse_generic_params()?
        } else {
            vec![]
        };
        let target = self.parse_type()?;
        let trait_type = if self.match_kind(&TokenKind::KwFor) {
            Some(target.clone())
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
        let mut methods = Vec::new();
        let mut _assoc_types = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.is_at_end() {
            if self.match_kind(&TokenKind::KwType) {
                _assoc_types.push(self.parse_assoc_type()?);
            } else {
                let visibility = if self.match_kind(&TokenKind::KwPub) {
                    Visibility::Public
                } else {
                    Visibility::Private
                };
                let is_async = self.match_kind(&TokenKind::KwAsync);
                self.consume(&TokenKind::KwFn, "expected 'fn'")?;
                methods.push(self.parse_function(visibility, vec![], is_async, false)?);
            }
        }
        self.consume(&TokenKind::RBrace, "expected '}'")?;
        Ok(ImplBlock {
            generics,
            target,
            trait_type,
            methods,
            where_clause,
            span,
        })
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
            let _ = self.parse_type()?;
            if self.match_kind(&TokenKind::Eq) {
                let _ = self.parse_expression()?;
            }
            Ok(GenericParam::Const(name))
        } else if matches!(self.peek().kind, TokenKind::Lifetime(_)) {
            // 生命周期参数：`struct Foo<'a> { ... }`
            let name = match &self.peek().kind {
                TokenKind::Lifetime(n) => n.clone(),
                _ => String::new(),
            };
            self.advance();
            if self.match_kind(&TokenKind::Colon) {
                // 生命周期约束：目前直接跳过
                while !matches!(
                    self.peek().kind,
                    TokenKind::Gt | TokenKind::Comma | TokenKind::Eof
                ) && !self.is_at_end()
                {
                    self.advance();
                }
            }
            Ok(GenericParam::Lifetime(name))
        } else {
            let name = self.consume_ident("expected generic parameter name")?;
            if self.match_kind(&TokenKind::Colon) {
                let _ = self.parse_type()?;
                while self.match_kind(&TokenKind::Plus) {
                    let _ = self.parse_type()?;
                }
            }
            if self.match_kind(&TokenKind::Eq) {
                let _ = self.parse_type()?;
            }
            Ok(GenericParam::Type(name))
        }
    }
}
