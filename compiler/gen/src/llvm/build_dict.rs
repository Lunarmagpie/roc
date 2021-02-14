use crate::debug_info_init;
use crate::llvm::build::{
    call_bitcode_fn, call_void_bitcode_fn, complex_bitcast, load_symbol, load_symbol_and_layout,
    set_name, Env, Scope,
};
use crate::llvm::convert::{self, as_const_zero, basic_type_from_layout, collection};
use crate::llvm::refcounting::{decrement_refcount_layout, increment_refcount_layout, Mode};
use inkwell::attributes::{Attribute, AttributeLoc};
use inkwell::types::BasicType;
use inkwell::values::{BasicValueEnum, FunctionValue, StructValue};
use inkwell::AddressSpace;
use roc_builtins::bitcode;
use roc_module::symbol::Symbol;
use roc_mono::layout::{Builtin, Layout, LayoutIds};

#[repr(u8)]
enum Alignment {
    Align16KeyFirst = 0,
    Align16ValueFirst = 1,
    Align8KeyFirst = 2,
    Align8ValueFirst = 3,
}

impl Alignment {
    fn from_key_value_layout(key: &Layout, value: &Layout, ptr_bytes: u32) -> Alignment {
        let key_align = key.alignment_bytes(ptr_bytes);
        let value_align = value.alignment_bytes(ptr_bytes);

        if key_align >= value_align {
            match key_align.max(value_align) {
                8 => Alignment::Align8KeyFirst,
                16 => Alignment::Align16KeyFirst,
                _ => unreachable!(),
            }
        } else {
            match key_align.max(value_align) {
                8 => Alignment::Align8ValueFirst,
                16 => Alignment::Align16ValueFirst,
                _ => unreachable!(),
            }
        }
    }
}

pub fn dict_len<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    scope: &Scope<'a, 'ctx>,
    dict_symbol: Symbol,
) -> BasicValueEnum<'ctx> {
    let ctx = env.context;

    let (_, dict_layout) = load_symbol_and_layout(scope, &dict_symbol);

    match dict_layout {
        Layout::Builtin(Builtin::Dict(_, _)) => {
            // let dict_as_int = dict_symbol_to_i128(env, scope, dict_symbol);
            let dict_as_zig_dict = dict_symbol_to_zig_dict(env, scope, dict_symbol);

            let dict_ptr = env
                .builder
                .build_alloca(dict_as_zig_dict.get_type(), "dict_ptr");
            env.builder.build_store(dict_ptr, dict_as_zig_dict);

            call_bitcode_fn(env, &[dict_ptr.into()], &bitcode::DICT_LEN)
        }
        Layout::Builtin(Builtin::EmptyDict) => ctx.i64_type().const_zero().into(),
        _ => unreachable!("Invalid layout given to Dict.len : {:?}", dict_layout),
    }
}

pub fn dict_empty<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    _scope: &Scope<'a, 'ctx>,
) -> BasicValueEnum<'ctx> {
    // get the RocDict type defined by zig
    let roc_dict_type = env.module.get_struct_type("dict.RocDict").unwrap();

    // we must give a pointer for the bitcode function to write the result into
    let result_alloc = env.builder.build_alloca(roc_dict_type, "dict_empty");

    call_void_bitcode_fn(env, &[result_alloc.into()], &bitcode::DICT_EMPTY);

    let result = env
        .builder
        .build_load(result_alloc, "load_result")
        .into_struct_value();

    zig_dict_to_struct(env, result).into()
}

#[allow(clippy::too_many_arguments)]
pub fn dict_insert<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    dict: BasicValueEnum<'ctx>,
    key: BasicValueEnum<'ctx>,
    key_layout: &Layout<'a>,
    value: BasicValueEnum<'ctx>,
    value_layout: &Layout<'a>,
) -> BasicValueEnum<'ctx> {
    let builder = env.builder;

    let zig_dict_type = env.module.get_struct_type("dict.RocDict").unwrap();
    let u8_ptr = env.context.i8_type().ptr_type(AddressSpace::Generic);

    let dict_ptr = builder.build_alloca(zig_dict_type, "dict_ptr");
    let key_ptr = builder.build_alloca(key.get_type(), "key_ptr");
    let value_ptr = builder.build_alloca(value.get_type(), "value_ptr");

    env.builder
        .build_store(dict_ptr, struct_to_zig_dict(env, dict.into_struct_value()));
    env.builder.build_store(key_ptr, key);
    env.builder.build_store(value_ptr, value);

    let key_width = env
        .ptr_int()
        .const_int(key_layout.stack_size(env.ptr_bytes) as u64, false);

    let value_width = env
        .ptr_int()
        .const_int(value_layout.stack_size(env.ptr_bytes) as u64, false);

    let result_ptr = builder.build_alloca(zig_dict_type, "result_ptr");

    let alignment = Alignment::from_key_value_layout(key_layout, value_layout, env.ptr_bytes);
    let alignment_iv = env.context.i8_type().const_int(alignment as u64, false);

    let hash_fn = build_hash_wrapper(env, layout_ids, key_layout);
    let eq_fn = build_eq_wrapper(env, layout_ids, key_layout);

    let dec_key_fn = build_rc_wrapper(env, layout_ids, key_layout, Mode::Dec);
    let dec_value_fn = build_rc_wrapper(env, layout_ids, value_layout, Mode::Dec);

    call_void_bitcode_fn(
        env,
        &[
            dict_ptr.into(),
            alignment_iv.into(),
            env.builder.build_bitcast(key_ptr, u8_ptr, "to_u8_ptr"),
            key_width.into(),
            env.builder.build_bitcast(value_ptr, u8_ptr, "to_u8_ptr"),
            value_width.into(),
            hash_fn.as_global_value().as_pointer_value().into(),
            eq_fn.as_global_value().as_pointer_value().into(),
            dec_key_fn.as_global_value().as_pointer_value().into(),
            dec_value_fn.as_global_value().as_pointer_value().into(),
            result_ptr.into(),
        ],
        &bitcode::DICT_INSERT,
    );

    let result_ptr = env
        .builder
        .build_bitcast(
            result_ptr,
            convert::dict(env.context, env.ptr_bytes).ptr_type(AddressSpace::Generic),
            "to_roc_dict",
        )
        .into_pointer_value();

    env.builder.build_load(result_ptr, "load_result")
}

#[allow(clippy::too_many_arguments)]
pub fn dict_remove<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    dict: BasicValueEnum<'ctx>,
    key: BasicValueEnum<'ctx>,
    key_layout: &Layout<'a>,
    value_layout: &Layout<'a>,
) -> BasicValueEnum<'ctx> {
    let builder = env.builder;

    let zig_dict_type = env.module.get_struct_type("dict.RocDict").unwrap();
    let u8_ptr = env.context.i8_type().ptr_type(AddressSpace::Generic);

    let dict_ptr = builder.build_alloca(zig_dict_type, "dict_ptr");
    let key_ptr = builder.build_alloca(key.get_type(), "key_ptr");

    env.builder
        .build_store(dict_ptr, struct_to_zig_dict(env, dict.into_struct_value()));
    env.builder.build_store(key_ptr, key);

    let key_width = env
        .ptr_int()
        .const_int(key_layout.stack_size(env.ptr_bytes) as u64, false);

    let value_width = env
        .ptr_int()
        .const_int(value_layout.stack_size(env.ptr_bytes) as u64, false);

    let result_ptr = builder.build_alloca(zig_dict_type, "result_ptr");

    let alignment = Alignment::from_key_value_layout(key_layout, value_layout, env.ptr_bytes);
    let alignment_iv = env.context.i8_type().const_int(alignment as u64, false);

    let hash_fn = build_hash_wrapper(env, layout_ids, key_layout);
    let eq_fn = build_eq_wrapper(env, layout_ids, key_layout);

    let dec_key_fn = build_rc_wrapper(env, layout_ids, key_layout, Mode::Dec);
    let dec_value_fn = build_rc_wrapper(env, layout_ids, value_layout, Mode::Dec);

    call_void_bitcode_fn(
        env,
        &[
            dict_ptr.into(),
            alignment_iv.into(),
            env.builder.build_bitcast(key_ptr, u8_ptr, "to_u8_ptr"),
            key_width.into(),
            value_width.into(),
            hash_fn.as_global_value().as_pointer_value().into(),
            eq_fn.as_global_value().as_pointer_value().into(),
            dec_key_fn.as_global_value().as_pointer_value().into(),
            dec_value_fn.as_global_value().as_pointer_value().into(),
            result_ptr.into(),
        ],
        &bitcode::DICT_REMOVE,
    );

    let result_ptr = env
        .builder
        .build_bitcast(
            result_ptr,
            convert::dict(env.context, env.ptr_bytes).ptr_type(AddressSpace::Generic),
            "to_roc_dict",
        )
        .into_pointer_value();

    env.builder.build_load(result_ptr, "load_result")
}

#[allow(clippy::too_many_arguments)]
pub fn dict_contains<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    dict: BasicValueEnum<'ctx>,
    key: BasicValueEnum<'ctx>,
    key_layout: &Layout<'a>,
    value_layout: &Layout<'a>,
) -> BasicValueEnum<'ctx> {
    let builder = env.builder;

    let zig_dict_type = env.module.get_struct_type("dict.RocDict").unwrap();
    let u8_ptr = env.context.i8_type().ptr_type(AddressSpace::Generic);

    let dict_ptr = builder.build_alloca(zig_dict_type, "dict_ptr");
    let key_ptr = builder.build_alloca(key.get_type(), "key_ptr");

    env.builder
        .build_store(dict_ptr, struct_to_zig_dict(env, dict.into_struct_value()));
    env.builder.build_store(key_ptr, key);

    let key_width = env
        .ptr_int()
        .const_int(key_layout.stack_size(env.ptr_bytes) as u64, false);

    let value_width = env
        .ptr_int()
        .const_int(value_layout.stack_size(env.ptr_bytes) as u64, false);

    let alignment = Alignment::from_key_value_layout(key_layout, value_layout, env.ptr_bytes);
    let alignment_iv = env.context.i8_type().const_int(alignment as u64, false);

    let hash_fn = build_hash_wrapper(env, layout_ids, key_layout);
    let eq_fn = build_eq_wrapper(env, layout_ids, key_layout);

    call_bitcode_fn(
        env,
        &[
            dict_ptr.into(),
            alignment_iv.into(),
            env.builder.build_bitcast(key_ptr, u8_ptr, "to_u8_ptr"),
            key_width.into(),
            value_width.into(),
            hash_fn.as_global_value().as_pointer_value().into(),
            eq_fn.as_global_value().as_pointer_value().into(),
        ],
        &bitcode::DICT_CONTAINS,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn dict_get<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    dict: BasicValueEnum<'ctx>,
    key: BasicValueEnum<'ctx>,
    key_layout: &Layout<'a>,
    value_layout: &Layout<'a>,
) -> BasicValueEnum<'ctx> {
    let builder = env.builder;

    let zig_dict_type = env.module.get_struct_type("dict.RocDict").unwrap();
    let u8_ptr = env.context.i8_type().ptr_type(AddressSpace::Generic);

    let dict_ptr = builder.build_alloca(zig_dict_type, "dict_ptr");
    let key_ptr = builder.build_alloca(key.get_type(), "key_ptr");

    env.builder
        .build_store(dict_ptr, struct_to_zig_dict(env, dict.into_struct_value()));
    env.builder.build_store(key_ptr, key);

    let key_width = env
        .ptr_int()
        .const_int(key_layout.stack_size(env.ptr_bytes) as u64, false);

    let value_width = env
        .ptr_int()
        .const_int(value_layout.stack_size(env.ptr_bytes) as u64, false);

    let alignment = Alignment::from_key_value_layout(key_layout, value_layout, env.ptr_bytes);
    let alignment_iv = env.context.i8_type().const_int(alignment as u64, false);

    let hash_fn = build_hash_wrapper(env, layout_ids, key_layout);
    let eq_fn = build_eq_wrapper(env, layout_ids, key_layout);

    let inc_value_fn = build_rc_wrapper(env, layout_ids, value_layout, Mode::Inc(1));

    // { flag: bool, value: *const u8 }
    let result = call_bitcode_fn(
        env,
        &[
            dict_ptr.into(),
            alignment_iv.into(),
            env.builder.build_bitcast(key_ptr, u8_ptr, "to_u8_ptr"),
            key_width.into(),
            value_width.into(),
            hash_fn.as_global_value().as_pointer_value().into(),
            eq_fn.as_global_value().as_pointer_value().into(),
            inc_value_fn.as_global_value().as_pointer_value().into(),
        ],
        &bitcode::DICT_GET,
    )
    .into_struct_value();

    let flag = env
        .builder
        .build_extract_value(result, 1, "get_flag")
        .unwrap()
        .into_int_value();

    let value_u8_ptr = env
        .builder
        .build_extract_value(result, 0, "get_value_ptr")
        .unwrap()
        .into_pointer_value();

    let start_block = env.builder.get_insert_block().unwrap();
    let parent = start_block.get_parent().unwrap();

    let if_not_null = env.context.append_basic_block(parent, "if_not_null");
    let done_block = env.context.append_basic_block(parent, "done");

    let value_bt = basic_type_from_layout(env.arena, env.context, value_layout, env.ptr_bytes);
    let default = as_const_zero(&value_bt);

    env.builder
        .build_conditional_branch(flag, if_not_null, done_block);

    env.builder.position_at_end(if_not_null);
    let value_ptr = env
        .builder
        .build_bitcast(
            value_u8_ptr,
            value_bt.ptr_type(AddressSpace::Generic),
            "from_opaque",
        )
        .into_pointer_value();
    let loaded = env.builder.build_load(value_ptr, "load_value");
    env.builder.build_unconditional_branch(done_block);

    env.builder.position_at_end(done_block);
    let result_phi = env.builder.build_phi(value_bt, "result");

    result_phi.add_incoming(&[(&default, start_block), (&loaded, if_not_null)]);

    let value = result_phi.as_basic_value();

    let result = env
        .context
        .struct_type(&[value_bt, env.context.bool_type().into()], false)
        .const_zero();

    let result = env
        .builder
        .build_insert_value(result, flag, 1, "insert_flag")
        .unwrap();

    env.builder
        .build_insert_value(result, value, 0, "insert_value")
        .unwrap()
        .into_struct_value()
        .into()
}

#[allow(clippy::too_many_arguments)]
pub fn dict_elements_rc<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    dict: BasicValueEnum<'ctx>,
    key_layout: &Layout<'a>,
    value_layout: &Layout<'a>,
    rc_operation: Mode,
) {
    let builder = env.builder;

    let zig_dict_type = env.module.get_struct_type("dict.RocDict").unwrap();

    let dict_ptr = builder.build_alloca(zig_dict_type, "dict_ptr");
    env.builder
        .build_store(dict_ptr, struct_to_zig_dict(env, dict.into_struct_value()));

    let key_width = env
        .ptr_int()
        .const_int(key_layout.stack_size(env.ptr_bytes) as u64, false);

    let value_width = env
        .ptr_int()
        .const_int(value_layout.stack_size(env.ptr_bytes) as u64, false);

    let alignment = Alignment::from_key_value_layout(key_layout, value_layout, env.ptr_bytes);
    let alignment_iv = env.context.i8_type().const_int(alignment as u64, false);

    let inc_key_fn = build_rc_wrapper(env, layout_ids, key_layout, rc_operation);
    let inc_value_fn = build_rc_wrapper(env, layout_ids, value_layout, rc_operation);

    call_void_bitcode_fn(
        env,
        &[
            dict_ptr.into(),
            alignment_iv.into(),
            key_width.into(),
            value_width.into(),
            inc_key_fn.as_global_value().as_pointer_value().into(),
            inc_value_fn.as_global_value().as_pointer_value().into(),
        ],
        &bitcode::DICT_ELEMENTS_RC,
    );
}

#[allow(clippy::too_many_arguments)]
pub fn dict_keys<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    dict: BasicValueEnum<'ctx>,
    key_layout: &Layout<'a>,
    value_layout: &Layout<'a>,
) -> BasicValueEnum<'ctx> {
    let builder = env.builder;

    let zig_dict_type = env.module.get_struct_type("dict.RocDict").unwrap();
    let zig_list_type = env.module.get_struct_type("dict.RocList").unwrap();

    let dict_ptr = builder.build_alloca(zig_dict_type, "dict_ptr");
    env.builder
        .build_store(dict_ptr, struct_to_zig_dict(env, dict.into_struct_value()));

    let key_width = env
        .ptr_int()
        .const_int(key_layout.stack_size(env.ptr_bytes) as u64, false);

    let value_width = env
        .ptr_int()
        .const_int(value_layout.stack_size(env.ptr_bytes) as u64, false);

    let alignment = Alignment::from_key_value_layout(key_layout, value_layout, env.ptr_bytes);
    let alignment_iv = env.context.i8_type().const_int(alignment as u64, false);

    let inc_key_fn = build_rc_wrapper(env, layout_ids, key_layout, Mode::Inc(1));

    let list_ptr = builder.build_alloca(zig_list_type, "list_ptr");

    call_void_bitcode_fn(
        env,
        &[
            dict_ptr.into(),
            alignment_iv.into(),
            key_width.into(),
            value_width.into(),
            inc_key_fn.as_global_value().as_pointer_value().into(),
            list_ptr.into(),
        ],
        &bitcode::DICT_KEYS,
    );

    let list_ptr = env
        .builder
        .build_bitcast(
            list_ptr,
            collection(env.context, env.ptr_bytes).ptr_type(AddressSpace::Generic),
            "to_roc_list",
        )
        .into_pointer_value();

    env.builder.build_load(list_ptr, "load_keys_list")
}

#[allow(clippy::too_many_arguments)]
pub fn dict_union<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    dict1: BasicValueEnum<'ctx>,
    dict2: BasicValueEnum<'ctx>,
    key_layout: &Layout<'a>,
    value_layout: &Layout<'a>,
) -> BasicValueEnum<'ctx> {
    let builder = env.builder;

    let zig_dict_type = env.module.get_struct_type("dict.RocDict").unwrap();

    let dict1_ptr = builder.build_alloca(zig_dict_type, "dict_ptr");
    let dict2_ptr = builder.build_alloca(zig_dict_type, "dict_ptr");

    env.builder.build_store(
        dict1_ptr,
        struct_to_zig_dict(env, dict1.into_struct_value()),
    );

    env.builder.build_store(
        dict2_ptr,
        struct_to_zig_dict(env, dict2.into_struct_value()),
    );

    let key_width = env
        .ptr_int()
        .const_int(key_layout.stack_size(env.ptr_bytes) as u64, false);

    let value_width = env
        .ptr_int()
        .const_int(value_layout.stack_size(env.ptr_bytes) as u64, false);

    let alignment = Alignment::from_key_value_layout(key_layout, value_layout, env.ptr_bytes);
    let alignment_iv = env.context.i8_type().const_int(alignment as u64, false);

    let hash_fn = build_hash_wrapper(env, layout_ids, key_layout);
    let eq_fn = build_eq_wrapper(env, layout_ids, key_layout);

    let inc_key_fn = build_rc_wrapper(env, layout_ids, key_layout, Mode::Inc(1));
    let inc_value_fn = build_rc_wrapper(env, layout_ids, value_layout, Mode::Inc(1));

    let output_ptr = builder.build_alloca(zig_dict_type, "output_ptr");

    call_void_bitcode_fn(
        env,
        &[
            dict1_ptr.into(),
            dict2_ptr.into(),
            alignment_iv.into(),
            key_width.into(),
            value_width.into(),
            hash_fn.as_global_value().as_pointer_value().into(),
            eq_fn.as_global_value().as_pointer_value().into(),
            inc_key_fn.as_global_value().as_pointer_value().into(),
            inc_value_fn.as_global_value().as_pointer_value().into(),
            output_ptr.into(),
        ],
        &bitcode::DICT_UNION,
    );

    let output_ptr = env
        .builder
        .build_bitcast(
            output_ptr,
            convert::dict(env.context, env.ptr_bytes).ptr_type(AddressSpace::Generic),
            "to_roc_dict",
        )
        .into_pointer_value();

    env.builder.build_load(output_ptr, "load_output_ptr")
}

#[allow(clippy::too_many_arguments)]
pub fn dict_intersection<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    dict1: BasicValueEnum<'ctx>,
    dict2: BasicValueEnum<'ctx>,
    key_layout: &Layout<'a>,
    value_layout: &Layout<'a>,
) -> BasicValueEnum<'ctx> {
    let builder = env.builder;

    let zig_dict_type = env.module.get_struct_type("dict.RocDict").unwrap();

    let dict1_ptr = builder.build_alloca(zig_dict_type, "dict_ptr");
    let dict2_ptr = builder.build_alloca(zig_dict_type, "dict_ptr");

    env.builder.build_store(
        dict1_ptr,
        struct_to_zig_dict(env, dict1.into_struct_value()),
    );

    env.builder.build_store(
        dict2_ptr,
        struct_to_zig_dict(env, dict2.into_struct_value()),
    );

    let key_width = env
        .ptr_int()
        .const_int(key_layout.stack_size(env.ptr_bytes) as u64, false);

    let value_width = env
        .ptr_int()
        .const_int(value_layout.stack_size(env.ptr_bytes) as u64, false);

    let alignment = Alignment::from_key_value_layout(key_layout, value_layout, env.ptr_bytes);
    let alignment_iv = env.context.i8_type().const_int(alignment as u64, false);

    let hash_fn = build_hash_wrapper(env, layout_ids, key_layout);
    let eq_fn = build_eq_wrapper(env, layout_ids, key_layout);

    let dec_key_fn = build_rc_wrapper(env, layout_ids, key_layout, Mode::Dec);
    let dec_value_fn = build_rc_wrapper(env, layout_ids, value_layout, Mode::Dec);

    let output_ptr = builder.build_alloca(zig_dict_type, "output_ptr");

    call_void_bitcode_fn(
        env,
        &[
            dict1_ptr.into(),
            dict2_ptr.into(),
            alignment_iv.into(),
            key_width.into(),
            value_width.into(),
            hash_fn.as_global_value().as_pointer_value().into(),
            eq_fn.as_global_value().as_pointer_value().into(),
            dec_key_fn.as_global_value().as_pointer_value().into(),
            dec_value_fn.as_global_value().as_pointer_value().into(),
            output_ptr.into(),
        ],
        &bitcode::DICT_INTERSECTION,
    );

    let output_ptr = env
        .builder
        .build_bitcast(
            output_ptr,
            convert::dict(env.context, env.ptr_bytes).ptr_type(AddressSpace::Generic),
            "to_roc_dict",
        )
        .into_pointer_value();

    env.builder.build_load(output_ptr, "load_output_ptr")
}

#[allow(clippy::too_many_arguments)]
pub fn dict_difference<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    dict1: BasicValueEnum<'ctx>,
    dict2: BasicValueEnum<'ctx>,
    key_layout: &Layout<'a>,
    value_layout: &Layout<'a>,
) -> BasicValueEnum<'ctx> {
    let builder = env.builder;

    let zig_dict_type = env.module.get_struct_type("dict.RocDict").unwrap();

    let dict1_ptr = builder.build_alloca(zig_dict_type, "dict_ptr");
    let dict2_ptr = builder.build_alloca(zig_dict_type, "dict_ptr");

    env.builder.build_store(
        dict1_ptr,
        struct_to_zig_dict(env, dict1.into_struct_value()),
    );

    env.builder.build_store(
        dict2_ptr,
        struct_to_zig_dict(env, dict2.into_struct_value()),
    );

    let key_width = env
        .ptr_int()
        .const_int(key_layout.stack_size(env.ptr_bytes) as u64, false);

    let value_width = env
        .ptr_int()
        .const_int(value_layout.stack_size(env.ptr_bytes) as u64, false);

    let alignment = Alignment::from_key_value_layout(key_layout, value_layout, env.ptr_bytes);
    let alignment_iv = env.context.i8_type().const_int(alignment as u64, false);

    let hash_fn = build_hash_wrapper(env, layout_ids, key_layout);
    let eq_fn = build_eq_wrapper(env, layout_ids, key_layout);

    let dec_key_fn = build_rc_wrapper(env, layout_ids, key_layout, Mode::Dec);
    let dec_value_fn = build_rc_wrapper(env, layout_ids, value_layout, Mode::Dec);

    let output_ptr = builder.build_alloca(zig_dict_type, "output_ptr");

    call_void_bitcode_fn(
        env,
        &[
            dict1_ptr.into(),
            dict2_ptr.into(),
            alignment_iv.into(),
            key_width.into(),
            value_width.into(),
            hash_fn.as_global_value().as_pointer_value().into(),
            eq_fn.as_global_value().as_pointer_value().into(),
            dec_key_fn.as_global_value().as_pointer_value().into(),
            dec_value_fn.as_global_value().as_pointer_value().into(),
            output_ptr.into(),
        ],
        &bitcode::DICT_DIFFERENCE,
    );

    let output_ptr = env
        .builder
        .build_bitcast(
            output_ptr,
            convert::dict(env.context, env.ptr_bytes).ptr_type(AddressSpace::Generic),
            "to_roc_dict",
        )
        .into_pointer_value();

    env.builder.build_load(output_ptr, "load_output_ptr")
}

#[allow(clippy::too_many_arguments)]
pub fn dict_values<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    dict: BasicValueEnum<'ctx>,
    key_layout: &Layout<'a>,
    value_layout: &Layout<'a>,
) -> BasicValueEnum<'ctx> {
    let builder = env.builder;

    let zig_dict_type = env.module.get_struct_type("dict.RocDict").unwrap();
    let zig_list_type = env.module.get_struct_type("dict.RocList").unwrap();

    let dict_ptr = builder.build_alloca(zig_dict_type, "dict_ptr");
    env.builder
        .build_store(dict_ptr, struct_to_zig_dict(env, dict.into_struct_value()));

    let key_width = env
        .ptr_int()
        .const_int(key_layout.stack_size(env.ptr_bytes) as u64, false);

    let value_width = env
        .ptr_int()
        .const_int(value_layout.stack_size(env.ptr_bytes) as u64, false);

    let alignment = Alignment::from_key_value_layout(key_layout, value_layout, env.ptr_bytes);
    let alignment_iv = env.context.i8_type().const_int(alignment as u64, false);

    let inc_value_fn = build_rc_wrapper(env, layout_ids, value_layout, Mode::Inc(1));

    let list_ptr = builder.build_alloca(zig_list_type, "list_ptr");

    call_void_bitcode_fn(
        env,
        &[
            dict_ptr.into(),
            alignment_iv.into(),
            key_width.into(),
            value_width.into(),
            inc_value_fn.as_global_value().as_pointer_value().into(),
            list_ptr.into(),
        ],
        &bitcode::DICT_VALUES,
    );

    let list_ptr = env
        .builder
        .build_bitcast(
            list_ptr,
            collection(env.context, env.ptr_bytes).ptr_type(AddressSpace::Generic),
            "to_roc_list",
        )
        .into_pointer_value();

    env.builder.build_load(list_ptr, "load_keys_list")
}

fn build_hash_wrapper<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    layout: &Layout<'a>,
) -> FunctionValue<'ctx> {
    let block = env.builder.get_insert_block().expect("to be in a function");
    let di_location = env.builder.get_current_debug_location().unwrap();

    let symbol = Symbol::GENERIC_HASH_REF;
    let fn_name = layout_ids
        .get(symbol, &layout)
        .to_symbol_string(symbol, &env.interns);

    let function_value = match env.module.get_function(fn_name.as_str()) {
        Some(function_value) => function_value,
        None => {
            let seed_type = env.context.i64_type();
            let arg_type = env.context.i8_type().ptr_type(AddressSpace::Generic);

            let function_value = crate::llvm::refcounting::build_header_help(
                env,
                &fn_name,
                seed_type.into(),
                &[seed_type.into(), arg_type.into()],
            );

            let kind_id = Attribute::get_named_enum_kind_id("alwaysinline");
            debug_assert!(kind_id > 0);
            let attr = env.context.create_enum_attribute(kind_id, 1);
            function_value.add_attribute(AttributeLoc::Function, attr);

            let entry = env.context.append_basic_block(function_value, "entry");
            env.builder.position_at_end(entry);

            debug_info_init!(env, function_value);

            let mut it = function_value.get_param_iter();
            let seed_arg = it.next().unwrap().into_int_value();
            let value_ptr = it.next().unwrap().into_pointer_value();

            set_name(seed_arg.into(), Symbol::ARG_1.ident_string(&env.interns));
            set_name(value_ptr.into(), Symbol::ARG_2.ident_string(&env.interns));

            let value_type = basic_type_from_layout(env.arena, env.context, layout, env.ptr_bytes)
                .ptr_type(AddressSpace::Generic);

            let value_cast = env
                .builder
                .build_bitcast(value_ptr, value_type, "load_opaque")
                .into_pointer_value();

            let val_arg = env.builder.build_load(value_cast, "load_opaque");

            let result =
                crate::llvm::build_hash::generic_hash(env, layout_ids, seed_arg, val_arg, layout);

            env.builder.build_return(Some(&result));

            function_value
        }
    };

    env.builder.position_at_end(block);
    env.builder
        .set_current_debug_location(env.context, di_location);

    function_value
}

fn build_eq_wrapper<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    layout: &Layout<'a>,
) -> FunctionValue<'ctx> {
    let block = env.builder.get_insert_block().expect("to be in a function");
    let di_location = env.builder.get_current_debug_location().unwrap();

    let symbol = Symbol::GENERIC_EQ_REF;
    let fn_name = layout_ids
        .get(symbol, &layout)
        .to_symbol_string(symbol, &env.interns);

    let function_value = match env.module.get_function(fn_name.as_str()) {
        Some(function_value) => function_value,
        None => {
            let arg_type = env.context.i8_type().ptr_type(AddressSpace::Generic);

            let function_value = crate::llvm::refcounting::build_header_help(
                env,
                &fn_name,
                env.context.bool_type().into(),
                &[arg_type.into(), arg_type.into()],
            );

            let kind_id = Attribute::get_named_enum_kind_id("alwaysinline");
            debug_assert!(kind_id > 0);
            let attr = env.context.create_enum_attribute(kind_id, 1);
            function_value.add_attribute(AttributeLoc::Function, attr);

            let entry = env.context.append_basic_block(function_value, "entry");
            env.builder.position_at_end(entry);

            debug_info_init!(env, function_value);

            let mut it = function_value.get_param_iter();
            let value_ptr1 = it.next().unwrap().into_pointer_value();
            let value_ptr2 = it.next().unwrap().into_pointer_value();

            set_name(value_ptr1.into(), Symbol::ARG_1.ident_string(&env.interns));
            set_name(value_ptr2.into(), Symbol::ARG_2.ident_string(&env.interns));

            let value_type = basic_type_from_layout(env.arena, env.context, layout, env.ptr_bytes)
                .ptr_type(AddressSpace::Generic);

            let value_cast1 = env
                .builder
                .build_bitcast(value_ptr1, value_type, "load_opaque")
                .into_pointer_value();

            let value_cast2 = env
                .builder
                .build_bitcast(value_ptr2, value_type, "load_opaque")
                .into_pointer_value();

            let value1 = env.builder.build_load(value_cast1, "load_opaque");
            let value2 = env.builder.build_load(value_cast2, "load_opaque");

            let result =
                crate::llvm::compare::generic_eq(env, layout_ids, value1, value2, layout, layout);

            env.builder.build_return(Some(&result));

            function_value
        }
    };

    env.builder.position_at_end(block);
    env.builder
        .set_current_debug_location(env.context, di_location);

    function_value
}

fn build_rc_wrapper<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    layout: &Layout<'a>,
    rc_operation: Mode,
) -> FunctionValue<'ctx> {
    let block = env.builder.get_insert_block().expect("to be in a function");
    let di_location = env.builder.get_current_debug_location().unwrap();

    let symbol = Symbol::GENERIC_RC_REF;
    let fn_name = layout_ids
        .get(symbol, &layout)
        .to_symbol_string(symbol, &env.interns);

    let fn_name = match rc_operation {
        Mode::Inc(n) => format!("{}_inc_{}", fn_name, n),
        Mode::Dec => format!("{}_dec", fn_name),
    };

    let function_value = match env.module.get_function(fn_name.as_str()) {
        Some(function_value) => function_value,
        None => {
            let arg_type = env.context.i8_type().ptr_type(AddressSpace::Generic);

            let function_value = crate::llvm::refcounting::build_header_help(
                env,
                &fn_name,
                env.context.void_type().into(),
                &[arg_type.into()],
            );

            let kind_id = Attribute::get_named_enum_kind_id("alwaysinline");
            debug_assert!(kind_id > 0);
            let attr = env.context.create_enum_attribute(kind_id, 1);
            function_value.add_attribute(AttributeLoc::Function, attr);

            let entry = env.context.append_basic_block(function_value, "entry");
            env.builder.position_at_end(entry);

            debug_info_init!(env, function_value);

            let mut it = function_value.get_param_iter();
            let value_ptr = it.next().unwrap().into_pointer_value();

            set_name(value_ptr.into(), Symbol::ARG_1.ident_string(&env.interns));

            let value_type = basic_type_from_layout(env.arena, env.context, layout, env.ptr_bytes)
                .ptr_type(AddressSpace::Generic);

            let value_cast = env
                .builder
                .build_bitcast(value_ptr, value_type, "load_opaque")
                .into_pointer_value();

            let value = env.builder.build_load(value_cast, "load_opaque");

            match rc_operation {
                Mode::Inc(n) => {
                    increment_refcount_layout(env, function_value, layout_ids, n, value, layout);
                }
                Mode::Dec => {
                    decrement_refcount_layout(env, function_value, layout_ids, value, layout);
                }
            }

            env.builder.build_return(None);

            function_value
        }
    };

    env.builder.position_at_end(block);
    env.builder
        .set_current_debug_location(env.context, di_location);

    function_value
}

fn dict_symbol_to_zig_dict<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    scope: &Scope<'a, 'ctx>,
    symbol: Symbol,
) -> StructValue<'ctx> {
    let dict = load_symbol(scope, &symbol);

    let zig_dict_type = env.module.get_struct_type("dict.RocDict").unwrap();

    complex_bitcast(&env.builder, dict, zig_dict_type.into(), "dict_to_zig_dict")
        .into_struct_value()
}

fn zig_dict_to_struct<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    zig_dict: StructValue<'ctx>,
) -> StructValue<'ctx> {
    complex_bitcast(
        env.builder,
        zig_dict.into(),
        crate::llvm::convert::dict(env.context, env.ptr_bytes).into(),
        "to_zig_dict",
    )
    .into_struct_value()
}

fn struct_to_zig_dict<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    struct_dict: StructValue<'ctx>,
) -> StructValue<'ctx> {
    // get the RocStr type defined by zig
    let zig_dict_type = env.module.get_struct_type("dict.RocDict").unwrap();

    complex_bitcast(
        env.builder,
        struct_dict.into(),
        zig_dict_type.into(),
        "to_zig_dict",
    )
    .into_struct_value()
}
