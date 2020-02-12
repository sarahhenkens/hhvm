// Copyright (c) Facebook, Inc. and its affiliates.
//
// This source code is licensed under the MIT license found in the
// LICENSE file in the "hack" directory of this source tree.

use closure_convert_rust as closure_convert;
use emit_attribute_rust as emit_attribute;
use emit_body_rust as emit_body;
use emit_fatal_rust as emit_fatal;
use emit_method_rust as emit_method;
use emit_property_rust as emit_property;
use emit_type_constant_rust as emit_type_constant;
use emit_type_hint_rust as emit_type_hint;
use env::{emitter::Emitter, Env};
use hhas_attribute_rust as hhas_attribute;
use hhas_class_rust::{HhasClass, HhasClassFlags, TraitReqKind};
use hhas_property_rust::HhasProperty;
use hhas_type_const::HhasTypeConstant;
use hhas_xhp_attribute_rust::HhasXhpAttribute;
use hhbc_id::{class, Id};
use hhbc_id_rust as hhbc_id;
use instruction_sequence_rust::Error::Unrecoverable;
use instruction_sequence_rust::Result;
use naming_special_names_rust as special_names;
use oxidized::{ast as tast, namespace_env};

use std::collections::BTreeMap;

fn from_extends(is_enum: bool, extends: &Vec<tast::Hint>) -> Option<hhbc_id::class::Type> {
    if is_enum {
        Some(hhbc_id::class::from_raw_string("HH\\BuiltinEnum"))
    } else {
        extends.first().map(|x| emit_type_hint::hint_to_class(x))
    }
}

fn from_implements(implements: &Vec<tast::Hint>) -> Vec<hhbc_id::class::Type> {
    implements
        .iter()
        .map(|x| emit_type_hint::hint_to_class(x))
        .collect()
}

fn from_type_constant<'a>(
    emitter: &mut Emitter,
    tc: &'a tast::ClassTypeconst,
) -> Result<HhasTypeConstant> {
    use tast::TypeconstAbstractKind::*;
    let name = tc.name.1.to_string();

    let initializer = match (&tc.abstract_, &tc.type_) {
        (TCAbstract(None), _) | (TCPartiallyAbstract, None) | (TCConcrete, None) => None,
        (TCAbstract(Some(init)), _)
        | (TCPartiallyAbstract, Some(init))
        | (TCConcrete, Some(init)) => {
            // TODO: Deal with the constraint
            // Type constants do not take type vars hence tparams:[]
            Some(emit_type_constant::hint_to_type_constant(
                emitter.options(),
                &vec![],
                &BTreeMap::new(),
                init,
                false,
                false,
            )?)
        }
    };

    Ok(HhasTypeConstant { name, initializer })
}

fn from_class_elt_classvars<'a>(
    emitter: &mut Emitter,
    namespace: &namespace_env::Env,
    ast_class: &'a tast::Class_,
    class_is_const: bool,
    tparams: &[&str],
) -> Result<Vec<HhasProperty<'a>>> {
    // TODO: we need to emit doc comments for each property,
    // not one per all properties on the same line
    // The doc comment is only for the first name in the list.
    // Currently this is organized in the ast_to_nast module
    ast_class
        .vars
        .iter()
        .map(|cv| {
            let hint = if cv.is_promoted_variadic {
                None
            } else {
                cv.type_.1.as_ref()
            };

            emit_property::from_ast(
                emitter,
                ast_class,
                namespace,
                tparams,
                class_is_const,
                emit_property::FromAstArgs {
                    user_attributes: &cv.user_attributes,
                    id: &cv.id,
                    initial_value: &cv.expr,
                    typehint: hint,
                    // Doc comments are weird. T40098274
                    doc_comment: cv.doc_comment.clone(),
                    visibility: cv.visibility, // This used to be cv_kinds
                    is_static: cv.is_static,
                    is_abstract: cv.abstract_,
                },
            )
        })
        .collect::<Result<Vec<_>>>()
}

fn from_class_elt_requirements<'a>(
    class_: &'a tast::Class_,
) -> Vec<(hhbc_id::class::Type, TraitReqKind)> {
    class_
        .reqs
        .iter()
        .map(|(h, is_extends)| {
            let kind = if *is_extends {
                TraitReqKind::MustExtend
            } else {
                TraitReqKind::MustImplement
            };
            (emit_type_hint::hint_to_class(h), kind)
        })
        .collect()
}

fn from_class_elt_typeconsts<'a>(
    emitter: &mut Emitter,
    class_: &'a tast::Class_,
) -> Result<Vec<HhasTypeConstant>> {
    class_
        .typeconsts
        .iter()
        .map(|x| from_type_constant(emitter, x))
        .collect()
}

fn from_enum_type(opt: Option<&tast::Enum_>) -> Result<Option<hhas_type::Info>> {
    use hhas_type::constraint::*;
    opt.map(|e| {
        let type_info_user_type = Some(emit_type_hint::fmt_hint(&[], true, &e.base)?);
        let type_info_type_constraint = Type::make(None, Flags::EXTENDED_HINT);
        Ok(hhas_type::Info::make(
            type_info_user_type,
            type_info_type_constraint,
        ))
    })
    .transpose()
}

pub fn emit_class<'a>(
    emitter: &mut Emitter,
    ast_class: &'a tast::Class_,
    hoisted: closure_convert::HoistKind,
) -> Result<HhasClass<'a>> {
    let namespace = &ast_class.namespace;
    // TODO(hrust): validate_class_name
    let _env = Env::make_class_env(ast_class);
    // TODO: communicate this without looking at the name
    let is_closure_class = ast_class.name.1.starts_with("Closure$");

    let mut attributes = emit_attribute::from_asts(emitter, namespace, &ast_class.user_attributes)?;
    if !is_closure_class {
        attributes
            .extend(emit_attribute::add_reified_attribute(&ast_class.tparams.list).into_iter());
        // TODO(hrust): add_reified_parent_attribute
    }

    let is_const = hhas_attribute::has_const(&attributes);
    // In the future, we intend to set class_no_dynamic_props independently from
    // class_is_const, but for now class_is_const is the only thing that turns
    // it on.
    let no_dynamic_props = is_const;
    let name = class::Type::from_ast_name(&ast_class.name.1);
    let is_trait = ast_class.kind == tast::ClassKind::Ctrait;
    let is_interface = ast_class.kind == tast::ClassKind::Cinterface;

    let uses = ast_class
        .uses
        .iter()
        .filter_map(|x| match x.1.as_ref() {
            tast::Hint_::Happly(tast::Id(p, name), _) => {
                if is_interface {
                    Some(Err(emit_fatal_rust::raise_fatal_parse(
                        p,
                        "Interfaces cannot use traits",
                    )))
                } else {
                    Some(Ok(name.into()))
                }
            }
            _ => None,
        })
        .collect::<Result<_>>()?;

    let elaborate_namespace_id = |x: &'a tast::Id| hhbc_id::class::Type::from_ast_name(x.name());
    let use_aliases = ast_class
        .use_as_alias
        .iter()
        .map(|tast::UseAsAlias(ido1, id, ido2, vis)| {
            let id1 = ido1.as_ref().map(|x| elaborate_namespace_id(x));
            let id2 = ido2.as_ref().map(|x| (&x.1).into());
            (id1, (&id.1).into(), id2, vis)
        })
        .collect();

    let use_precedences = ast_class
        .insteadof_alias
        .iter()
        .map(|tast::InsteadofAlias(id1, id2, ids)| {
            let id1 = elaborate_namespace_id(&id1);
            let id2 = &id2.1;
            let ids = ids.iter().map(|x| elaborate_namespace_id(x)).collect();
            (id1, id2.into(), ids)
        })
        .collect();
    let string_of_trait = |trait_: &'a tast::Hint| {
        use tast::Hint_::*;
        match trait_.1.as_ref() {
            // TODO: Currently, names are not elaborated.
            // Names should be elaborated if this feature is to be supported
            // T56629465
            Happly(tast::Id(_, trait_), _) => Ok(trait_.into()),
            // Happly converted from naming
            Hprim(p) => Ok(emit_type_hint::prim_to_string(p).into()),
            Hany | Herr => Err(Unrecoverable(
                "I'm convinced that this should be an error caught in naming".into(),
            )),
            Hmixed => Ok(special_names::typehints::MIXED.into()),
            Hnonnull => Ok(special_names::typehints::NONNULL.into()),
            Habstr(s) => Ok(s.into()),
            Harray(_, _) => Ok(special_names::typehints::ARRAY.into()),
            Hdarray(_, _) => Ok(special_names::typehints::DARRAY.into()),
            Hvarray(_) => Ok(special_names::typehints::VARRAY.into()),
            HvarrayOrDarray(_, _) => Ok(special_names::typehints::VARRAY_OR_DARRAY.into()),
            Hthis => Ok(special_names::typehints::THIS.into()),
            Hdynamic => Ok(special_names::typehints::DYNAMIC.into()),
            _ => Err(Unrecoverable("TODO Fail gracefully here".into())),
        }
    };
    let method_trait_resolutions: Vec<(_, class::Type)> = ast_class
        .method_redeclarations
        .iter()
        .map(|mtr| Ok((mtr, string_of_trait(&mtr.trait_)?)))
        .collect::<Result<_>>()?;

    let enum_type = if ast_class.kind == tast::ClassKind::Cenum {
        from_enum_type(ast_class.enum_.as_ref())?
    } else {
        None
    };
    let _xhp_attributes: Vec<_> = ast_class
        .xhp_attrs
        .iter()
        .map(
            |tast::XhpAttr(type_, class_var, tag, maybe_enum)| HhasXhpAttribute {
                type_: type_.1.as_ref(),
                class_var,
                tag: *tag,
                maybe_enum: maybe_enum.as_ref(),
            },
        )
        .collect();

    let _xhp_children = ast_class.xhp_children.first().map(|(p, sl)| (p, vec![sl]));
    let _xhp_categories: Option<(_, Vec<_>)> = ast_class
        .xhp_category
        .as_ref()
        .map(|(p, c)| (p, c.iter().map(|x| &x.1).collect()));

    let is_abstract = ast_class.kind == tast::ClassKind::Cabstract;
    let is_final = ast_class.final_ || is_trait || enum_type.is_some();
    let is_sealed = hhas_attribute::has_sealed(&attributes);

    let tparams: Vec<&str> = ast_class
        .tparams
        .list
        .iter()
        .map(|x| x.name.1.as_ref())
        .collect();

    let base = if is_interface {
        None
    } else {
        from_extends(enum_type.is_some(), &ast_class.extends)
    };

    if base
        .as_ref()
        .map(|cls| cls.to_raw_string().eq_ignore_ascii_case("closure") && !is_closure_class)
        .unwrap_or(false)
    {
        Err(emit_fatal::raise_fatal_runtime(
            &ast_class.name.0,
            "Class cannot extend Closure".to_string(),
        ))?;
    }
    let implements = if is_interface {
        &ast_class.extends
    } else {
        &ast_class.implements
    };
    let implements = from_implements(implements);
    let span = ast_class.span.clone().into();

    let properties = from_class_elt_classvars(emitter, namespace, &ast_class, is_const, &tparams)?;
    let requirements = from_class_elt_requirements(ast_class);

    let type_constants = from_class_elt_typeconsts(emitter, ast_class)?;
    let upper_bounds = if emitter.options().enforce_generic_ub() {
        emit_body::emit_generics_upper_bounds(&ast_class.tparams.list, false)
    } else {
        vec![]
    };

    let methods = emit_method::from_asts(emitter, ast_class, &ast_class.methods)?;

    let needs_no_reifiedinit = false; // TODO(hrust)
    let doc_comment = ast_class.doc_comment.clone();
    let is_xhp = ast_class.is_xhp || ast_class.has_xhp_keyword;

    let mut flags = HhasClassFlags::empty();
    flags.set(HhasClassFlags::IS_FINAL, is_final);
    flags.set(HhasClassFlags::IS_SEALED, is_sealed);
    flags.set(HhasClassFlags::IS_ABSTRACT, is_abstract);
    flags.set(HhasClassFlags::IS_INTERFACE, is_interface);
    flags.set(HhasClassFlags::IS_TRAIT, is_trait);
    flags.set(HhasClassFlags::IS_XHP, is_xhp);
    flags.set(HhasClassFlags::IS_CONST, is_const);
    flags.set(HhasClassFlags::NO_DYNAMIC_PROPS, no_dynamic_props);
    flags.set(HhasClassFlags::NEEDS_NO_REIFIEDINIT, needs_no_reifiedinit);

    Ok(HhasClass {
        attributes,
        base,
        implements,
        name,
        span,
        flags,
        doc_comment,
        uses,
        use_aliases,
        use_precedences,
        method_trait_resolutions,
        methods,
        enum_type,
        hoisted,
        upper_bounds,
        properties,
        requirements,
        type_constants,
    })
}

pub fn emit_classes_from_program<'a>(
    emitter: &mut Emitter,
    tast: &'a tast::Program,
) -> Result<Vec<HhasClass<'a>>> {
    tast.iter()
        .filter_map(|x| {
            if let tast::Def::Class(cd) = x {
                Some(emit_class(
                    emitter,
                    cd,
                    // TODO(hrust): pass the real hoist kind
                    closure_convert::HoistKind::TopLevel,
                ))
            } else {
                None
            }
        })
        .collect()
}