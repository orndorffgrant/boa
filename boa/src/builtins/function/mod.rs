//! This module implements the global `Function` object as well as creates Native Functions.
//!
//! Objects wrap `Function`s and expose them via call/construct slots.
//!
//! `The `Function` object is used for matching text with a pattern.
//!
//! More information:
//!  - [ECMAScript reference][spec]
//!  - [MDN documentation][mdn]
//!
//! [spec]: https://tc39.es/ecma262/#sec-function-objects
//! [mdn]: https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/Function

use std::{
    any::Any,
    borrow::Cow,
    fmt,
    ops::{Deref, DerefMut},
};

use dyn_clone::DynClone;
use gc::{Gc, GcCell};

use crate::{
    builtins::BuiltIn,
    context::StandardObjects,
    environment::lexical_environment::Environment,
    gc::{Finalize, Trace},
    object::JsObject,
    object::{internal_methods::get_prototype_from_constructor, NativeObject, ObjectData},
    property::Attribute,
    property::PropertyDescriptor,
    syntax::ast::node::declaration::Declaration,
    syntax::ast::node::{FormalParameter, RcStatementList},
    BoaProfiler, Context, JsResult, JsValue,
};
use crate::{object::Object, symbol::WellKnownSymbols};
use crate::{
    object::{ConstructorBuilder, FunctionBuilder},
    property::PropertyKey,
    JsString,
};
use crate::{
    object::{Ref, RefMut},
    value::IntegerOrInfinity,
};

use super::JsArgs;

pub(crate) mod arguments;
#[cfg(test)]
mod tests;

/// Type representing a native built-in function a.k.a. function pointer.
///
/// Native functions need to have this signature in order to
/// be callable from Javascript.
pub type NativeFunctionSignature = fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>;

// Allows restricting closures to only `Copy` ones.
// Used the sealed pattern to disallow external implementations
// of `DynCopy`.
mod sealed {
    pub trait Sealed {}
    impl<T: Copy> Sealed for T {}
}
pub trait DynCopy: sealed::Sealed {}
impl<T: Copy> DynCopy for T {}

/// Trait representing a native built-in closure.
///
/// Closures need to have this signature in order to
/// be callable from Javascript, but most of the time the compiler
/// is smart enough to correctly infer the types.
pub trait ClosureFunctionSignature:
    Fn(&JsValue, &[JsValue], Captures, &mut Context) -> JsResult<JsValue> + DynCopy + DynClone + 'static
{
}

impl<T> ClosureFunctionSignature for T where
    T: Fn(&JsValue, &[JsValue], Captures, &mut Context) -> JsResult<JsValue> + Copy + 'static
{
}

// Allows cloning Box<dyn ClosureFunctionSignature>
dyn_clone::clone_trait_object!(ClosureFunctionSignature);

#[derive(Debug, Trace, Finalize, PartialEq, Clone)]
pub enum ThisMode {
    Lexical,
    Strict,
    Global,
}

impl ThisMode {
    /// Returns `true` if the this mode is `Lexical`.
    pub fn is_lexical(&self) -> bool {
        matches!(self, Self::Lexical)
    }

    /// Returns `true` if the this mode is `Strict`.
    pub fn is_strict(&self) -> bool {
        matches!(self, Self::Strict)
    }

    /// Returns `true` if the this mode is `Global`.
    pub fn is_global(&self) -> bool {
        matches!(self, Self::Global)
    }
}

#[derive(Debug, Trace, Finalize, PartialEq, Clone)]
pub enum ConstructorKind {
    Base,
    Derived,
}

impl ConstructorKind {
    /// Returns `true` if the constructor kind is `Base`.
    pub fn is_base(&self) -> bool {
        matches!(self, Self::Base)
    }

    /// Returns `true` if the constructor kind is `Derived`.
    pub fn is_derived(&self) -> bool {
        matches!(self, Self::Derived)
    }
}

/// Wrapper for `Gc<GcCell<dyn NativeObject>>` that allows passing additional
/// captures through a `Copy` closure.
///
/// Any type implementing `Trace + Any + Debug`
/// can be used as a capture context, so you can pass e.g. a String,
/// a tuple or even a full struct.
///
/// You can cast to `Any` with `as_any`, `as_mut_any` and downcast
/// with `Any::downcast_ref` and `Any::downcast_mut` to recover the original
/// type.
#[derive(Clone, Debug, Trace, Finalize)]
pub struct Captures(Gc<GcCell<Box<dyn NativeObject>>>);

impl Captures {
    /// Creates a new capture context.
    pub(crate) fn new<T>(captures: T) -> Self
    where
        T: NativeObject,
    {
        Self(Gc::new(GcCell::new(Box::new(captures))))
    }

    /// Casts `Captures` to `Any`
    ///
    /// # Panics
    ///
    /// Panics if it's already borrowed as `&mut Any`
    pub fn as_any(&self) -> gc::GcCellRef<'_, dyn Any> {
        Ref::map(self.0.borrow(), |data| data.deref().as_any())
    }

    /// Mutably casts `Captures` to `Any`
    ///
    /// # Panics
    ///
    /// Panics if it's already borrowed as `&mut Any`
    pub fn as_mut_any(&self) -> gc::GcCellRefMut<'_, Box<dyn NativeObject>, dyn Any> {
        RefMut::map(self.0.borrow_mut(), |data| data.deref_mut().as_mut_any())
    }
}

/// Boa representation of a Function Object.
///
/// FunctionBody is specific to this interpreter, it will either be Rust code or JavaScript code (AST Node)
///
/// <https://tc39.es/ecma262/#sec-ecmascript-function-objects>
#[derive(Trace, Finalize)]
pub enum Function {
    Native {
        #[unsafe_ignore_trace]
        function: NativeFunctionSignature,
        constructor: bool,
    },
    Closure {
        #[unsafe_ignore_trace]
        function: Box<dyn ClosureFunctionSignature>,
        constructor: bool,
        captures: Captures,
    },
    Ordinary {
        constructor: bool,
        this_mode: ThisMode,
        body: RcStatementList,
        params: Box<[FormalParameter]>,
        environment: Environment,
    },
    #[cfg(feature = "vm")]
    VmOrdinary {
        code: Gc<crate::vm::CodeBlock>,
        environment: Environment,
    },
}

impl fmt::Debug for Function {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Function {{ ... }}")
    }
}

impl Function {
    // Adds the final rest parameters to the Environment as an array
    pub(crate) fn add_rest_param(
        param: &FormalParameter,
        index: usize,
        args_list: &[JsValue],
        context: &mut Context,
        local_env: &Environment,
    ) {
        use crate::builtins::Array;
        // Create array of values
        let array = Array::new_array(context);
        Array::add_to_array_object(&array, args_list.get(index..).unwrap_or_default(), context)
            .unwrap();

        let binding_params = param.run(Some(array), context).unwrap_or_default();
        for binding_items in binding_params.iter() {
            // Create binding
            local_env
                .create_mutable_binding(binding_items.0.as_ref(), false, true, context)
                .expect("Failed to create binding");

            // Set binding to value
            local_env
                .initialize_binding(
                    binding_items.0.as_ref(),
                    JsValue::new(binding_items.1.clone()),
                    context,
                )
                .expect("Failed to intialize binding");
        }
    }

    // Adds an argument to the environment
    pub(crate) fn add_arguments_to_environment(
        param: &FormalParameter,
        value: JsValue,
        local_env: &Environment,
        context: &mut Context,
    ) {
        let binding_params = param.run(Some(value), context).unwrap_or_default();
        for binding_items in binding_params.iter() {
            // Create binding
            local_env
                .create_mutable_binding(binding_items.0.as_ref(), false, true, context)
                .expect("Failed to create binding");

            // Set binding to value
            local_env
                .initialize_binding(
                    binding_items.0.as_ref(),
                    JsValue::new(binding_items.1.clone()),
                    context,
                )
                .expect("Failed to intialize binding");
        }
    }

    /// Returns true if the function object is a constructor.
    pub fn is_constructor(&self) -> bool {
        match self {
            Self::Native { constructor, .. } => *constructor,
            Self::Closure { constructor, .. } => *constructor,
            Self::Ordinary { constructor, .. } => *constructor,
            #[cfg(feature = "vm")]
            Self::VmOrdinary { code, .. } => code.constructor,
        }
    }
}

/// Creates a new member function of a `Object` or `prototype`.
///
/// A function registered using this macro can then be called from Javascript using:
///
/// parent.name()
///
/// See the javascript 'Number.toString()' as an example.
///
/// # Arguments
/// function: The function to register as a built in function.
/// name: The name of the function (how it will be called but without the ()).
/// parent: The object to register the function on, if the global object is used then the function is instead called as name()
///     without requiring the parent, see parseInt() as an example.
/// length: As described at <https://tc39.es/ecma262/#sec-function-instances-length>, The value of the "length" property is an integer that
///     indicates the typical number of arguments expected by the function. However, the language permits the function to be invoked with
///     some other number of arguments.
///
/// If no length is provided, the length will be set to 0.
// TODO: deprecate/remove this.
pub(crate) fn make_builtin_fn<N>(
    function: NativeFunctionSignature,
    name: N,
    parent: &JsObject,
    length: usize,
    interpreter: &Context,
) where
    N: Into<String>,
{
    let name = name.into();
    let _timer = BoaProfiler::global().start_event(&format!("make_builtin_fn: {}", &name), "init");

    let function = JsObject::from_proto_and_data(
        interpreter.standard_objects().function_object().prototype(),
        ObjectData::function(Function::Native {
            function,
            constructor: false,
        }),
    );
    let attribute = PropertyDescriptor::builder()
        .writable(false)
        .enumerable(false)
        .configurable(true);
    function.insert_property("length", attribute.clone().value(length));
    function.insert_property("name", attribute.value(name.as_str()));

    parent.clone().insert_property(
        name,
        PropertyDescriptor::builder()
            .value(function)
            .writable(true)
            .enumerable(false)
            .configurable(true),
    );
}

#[derive(Debug, Clone, Copy)]
pub struct BuiltInFunctionObject;

impl BuiltInFunctionObject {
    pub const LENGTH: usize = 1;

    fn constructor(
        new_target: &JsValue,
        _: &[JsValue],
        context: &mut Context,
    ) -> JsResult<JsValue> {
        let prototype =
            get_prototype_from_constructor(new_target, StandardObjects::function_object, context)?;

        let this = JsObject::from_proto_and_data(
            prototype,
            ObjectData::function(Function::Native {
                function: |_, _, _| Ok(JsValue::undefined()),
                constructor: true,
            }),
        );

        Ok(this.into())
    }

    /// `Function.prototype.apply ( thisArg, argArray )`
    ///
    /// The apply() method invokes self with the first argument as the `this` value
    /// and the rest of the arguments provided as an array (or an array-like object).
    ///
    /// More information:
    ///  - [MDN documentation][mdn]
    ///  - [ECMAScript reference][spec]
    ///
    /// [spec]: https://tc39.es/ecma262/#sec-function.prototype.apply
    /// [mdn]: https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/Function/apply
    fn apply(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
        // 1. Let func be the this value.
        // 2. If IsCallable(func) is false, throw a TypeError exception.
        let func = this.as_callable().ok_or_else(|| {
            context.construct_type_error(format!("{} is not a function", this.display()))
        })?;

        let this_arg = args.get_or_undefined(0);
        let arg_array = args.get_or_undefined(1);
        // 3. If argArray is undefined or null, then
        if arg_array.is_null_or_undefined() {
            // a. Perform PrepareForTailCall().
            // TODO?: 3.a. PrepareForTailCall

            // b. Return ? Call(func, thisArg).
            return func.call(this_arg, &[], context);
        }

        // 4. Let argList be ? CreateListFromArrayLike(argArray).
        let arg_list = arg_array.create_list_from_array_like(&[], context)?;

        // 5. Perform PrepareForTailCall().
        // TODO?: 5. PrepareForTailCall

        // 6. Return ? Call(func, thisArg, argList).
        func.call(this_arg, &arg_list, context)
    }

    /// `Function.prototype.bind ( thisArg, ...args )`
    ///
    /// The bind() method creates a new function that, when called, has its
    /// this keyword set to the provided value, with a given sequence of arguments
    /// preceding any provided when the new function is called.
    ///
    /// More information:
    ///  - [MDN documentation][mdn]
    ///  - [ECMAScript reference][spec]
    ///
    /// [spec]: https://tc39.es/ecma262/#sec-function.prototype.bind
    /// [mdn]: https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_objects/Function/bind
    fn bind(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
        // 1. Let Target be the this value.
        // 2. If IsCallable(Target) is false, throw a TypeError exception.
        let target = this.as_callable().ok_or_else(|| {
            context.construct_type_error("cannot bind `this` without a `[[Call]]` internal method")
        })?;

        let this_arg = args.get_or_undefined(0).clone();
        let bound_args = args.get(1..).unwrap_or_else(|| &[]).to_vec();
        let arg_count = bound_args.len() as i64;

        // 3. Let F be ? BoundFunctionCreate(Target, thisArg, args).
        let f = BoundFunction::create(target.clone(), this_arg, bound_args, context)?;

        // 4. Let L be 0.
        let mut l = JsValue::new(0);

        // 5. Let targetHasLength be ? HasOwnProperty(Target, "length").
        // 6. If targetHasLength is true, then
        if target.has_own_property("length", context)? {
            // a. Let targetLen be ? Get(Target, "length").
            let target_len = target.get("length", context)?;
            // b. If Type(targetLen) is Number, then
            if target_len.is_number() {
                // 1. Let targetLenAsInt be ! ToIntegerOrInfinity(targetLen).
                match target_len
                    .to_integer_or_infinity(context)
                    .expect("to_integer_or_infinity cannot fail for a number")
                {
                    // i. If targetLen is +∞𝔽, set L to +∞.
                    IntegerOrInfinity::PositiveInfinity => l = f64::INFINITY.into(),
                    // ii. Else if targetLen is -∞𝔽, set L to 0.
                    IntegerOrInfinity::NegativeInfinity => {}
                    // iii. Else,
                    IntegerOrInfinity::Integer(target_len) => {
                        // 2. Assert: targetLenAsInt is finite.
                        // 3. Let argCount be the number of elements in args.
                        // 4. Set L to max(targetLenAsInt - argCount, 0).
                        l = (target_len - arg_count).max(0).into();
                    }
                }
            }
        }

        // 7. Perform ! SetFunctionLength(F, L).
        f.define_property_or_throw(
            "length",
            PropertyDescriptor::builder()
                .value(l)
                .writable(false)
                .enumerable(false)
                .configurable(true),
            context,
        )
        .expect("defining the `length` property for a new object should not fail");

        // 8. Let targetName be ? Get(Target, "name").
        let target_name = target.get("name", context)?;

        // 9. If Type(targetName) is not String, set targetName to the empty String.
        let target_name = target_name
            .as_string()
            .map_or(JsString::new(""), Clone::clone);

        // 10. Perform SetFunctionName(F, targetName, "bound").
        set_function_name(&f, &target_name.into(), Some("bound"), context);

        // 11. Return F.
        Ok(f.into())
    }

    /// `Function.prototype.call ( thisArg, ...args )`
    ///
    /// The call() method calls a function with a given this value and arguments provided individually.
    ///
    /// More information:
    ///  - [MDN documentation][mdn]
    ///  - [ECMAScript reference][spec]
    ///
    /// [spec]: https://tc39.es/ecma262/#sec-function.prototype.call
    /// [mdn]: https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/Function/call
    fn call(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
        // 1. Let func be the this value.
        // 2. If IsCallable(func) is false, throw a TypeError exception.
        let func = this.as_callable().ok_or_else(|| {
            context.construct_type_error(format!("{} is not a function", this.display()))
        })?;
        let this_arg = args.get_or_undefined(0);

        // 3. Perform PrepareForTailCall().
        // TODO?: 3. Perform PrepareForTailCall

        // 4. Return ? Call(func, thisArg, args).
        func.call(this_arg, args.get(1..).unwrap_or(&[]), context)
    }

    #[allow(clippy::wrong_self_convention)]
    fn to_string(this: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
        let object = this.as_object().map(JsObject::borrow);
        let function = object
            .as_deref()
            .and_then(Object::as_function)
            .ok_or_else(|| context.construct_type_error("Not a function"))?;

        let name = {
            // Is there a case here where if there is no name field on a value
            // name should default to None? Do all functions have names set?
            let value = this
                .as_object()
                .expect("checked that `this` was an object above")
                .get("name", &mut *context)?;
            if value.is_null_or_undefined() {
                None
            } else {
                Some(value.to_string(context)?)
            }
        };

        match (function, name) {
            (
                Function::Native {
                    function: _,
                    constructor: _,
                },
                Some(name),
            ) => Ok(format!("function {}() {{\n  [native Code]\n}}", &name).into()),
            (Function::Ordinary { body, params, .. }, Some(name)) => {
                let arguments: String = {
                    let mut argument_list: Vec<Cow<'_, str>> = Vec::new();
                    for params_item in params.iter() {
                        let argument_item = match &params_item.declaration() {
                            Declaration::Identifier { ident, .. } => Cow::Borrowed(ident.as_ref()),
                            Declaration::Pattern(pattern) => {
                                Cow::Owned(format!("{{{}}}", pattern.idents().join(",")))
                            }
                        };
                        argument_list.push(argument_item);
                    }
                    argument_list.join(",")
                };

                let statement_list = &*body;
                // This is a kluge. The implementaion in browser seems to suggest that
                // the value here is printed exactly as defined in source. I'm not sure if
                // that's possible here, but for now here's a dumb heuristic that prints functions
                let is_multiline = {
                    let value = statement_list.to_string();
                    value.lines().count() > 1
                };
                if is_multiline {
                    Ok(
                        // ?? For some reason statement_list string implementation
                        // sticks a \n at the end no matter what
                        format!(
                            "{}({}) {{\n{}}}",
                            &name,
                            arguments,
                            statement_list.to_string()
                        )
                        .into(),
                    )
                } else {
                    Ok(format!(
                        "{}({}) {{{}}}",
                        &name,
                        arguments,
                        // The trim here is to remove a \n stuck at the end
                        // of the statement_list to_string method
                        statement_list.to_string().trim()
                    )
                    .into())
                }
            }

            _ => Ok("TODO".into()),
        }
    }

    /// `Function.prototype [ @@hasInstance ] ( V )`
    ///
    /// More information:
    ///  - [ECMAScript reference][spec]
    ///
    /// [spec]: https://tc39.es/ecma262/#sec-function.prototype-@@hasinstance
    fn has_instance(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
        // 1. Let F be the this value.
        // 2. Return ? OrdinaryHasInstance(F, V).
        Ok(JsValue::ordinary_has_instance(this, args.get_or_undefined(0), context)?.into())
    }

    fn prototype(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
        Ok(JsValue::undefined())
    }
}

impl BuiltIn for BuiltInFunctionObject {
    const NAME: &'static str = "Function";

    const ATTRIBUTE: Attribute = Attribute::WRITABLE
        .union(Attribute::NON_ENUMERABLE)
        .union(Attribute::CONFIGURABLE);

    fn init(context: &mut Context) -> JsValue {
        let _timer = BoaProfiler::global().start_event("function", "init");

        let function_prototype = context.standard_objects().function_object().prototype();
        FunctionBuilder::native(context, Self::prototype)
            .name("")
            .length(0)
            .constructor(false)
            .build_function_prototype(&function_prototype);

        let symbol_has_instance = WellKnownSymbols::has_instance();

        let has_instance = FunctionBuilder::native(context, Self::has_instance)
            .name("[Symbol.iterator]")
            .length(1)
            .constructor(false)
            .build();

        let function_object = ConstructorBuilder::with_standard_object(
            context,
            Self::constructor,
            context.standard_objects().function_object().clone(),
        )
        .name(Self::NAME)
        .length(Self::LENGTH)
        .method(Self::apply, "apply", 1)
        .method(Self::bind, "bind", 1)
        .method(Self::call, "call", 1)
        .method(Self::to_string, "toString", 0)
        .property(symbol_has_instance, has_instance, Attribute::default())
        .build();

        function_object.into()
    }
}

/// Abstract operation `SetFunctionName`
///
/// More information:
///  - [ECMAScript reference][spec]
///
/// [spec]: https://tc39.es/ecma262/#sec-setfunctionname
fn set_function_name(
    function: &JsObject,
    name: &PropertyKey,
    prefix: Option<&str>,
    context: &mut Context,
) {
    // 1. Assert: F is an extensible object that does not have a "name" own property.
    // 2. If Type(name) is Symbol, then
    let mut name = match name {
        PropertyKey::Symbol(sym) => {
            // a. Let description be name's [[Description]] value.
            if let Some(desc) = sym.description() {
                // c. Else, set name to the string-concatenation of "[", description, and "]".
                Cow::Owned(JsString::concat_array(&["[", &desc, "]"]))
            } else {
                // b. If description is undefined, set name to the empty String.
                Cow::Owned(JsString::new(""))
            }
        }
        PropertyKey::String(string) => Cow::Borrowed(string),
        PropertyKey::Index(index) => Cow::Owned(JsString::new(format!("{}", index))),
    };

    // 3. Else if name is a Private Name, then
    // a. Set name to name.[[Description]].
    // todo: implement Private Names

    // 4. If F has an [[InitialName]] internal slot, then
    // a. Set F.[[InitialName]] to name.
    // todo: implement [[InitialName]] for builtins

    // 5. If prefix is present, then
    if let Some(prefix) = prefix {
        name = Cow::Owned(JsString::concat_array(&[prefix, " ", &name]));
        // b. If F has an [[InitialName]] internal slot, then
        // i. Optionally, set F.[[InitialName]] to name.
        // todo: implement [[InitialName]] for builtins
    }

    // 6. Return ! DefinePropertyOrThrow(F, "name", PropertyDescriptor { [[Value]]: name,
    // [[Writable]]: false, [[Enumerable]]: false, [[Configurable]]: true }).
    function
        .define_property_or_throw(
            "name",
            PropertyDescriptor::builder()
                .value(name.into_owned())
                .writable(false)
                .enumerable(false)
                .configurable(true),
            context,
        )
        .expect("defining the `name` property must not fail per the spec");
}

/// Binds a `Function Object` when `bind` is called.
#[derive(Debug, Trace, Finalize)]
pub struct BoundFunction {
    target_function: JsObject,
    this: JsValue,
    args: Vec<JsValue>,
}

impl BoundFunction {
    /// Abstract operation `BoundFunctionCreate`
    ///
    /// More information:
    ///  - [ECMAScript reference][spec]
    ///
    /// [spec]: https://tc39.es/ecma262/#sec-boundfunctioncreate
    pub fn create(
        target_function: JsObject,
        this: JsValue,
        args: Vec<JsValue>,
        context: &mut Context,
    ) -> JsResult<JsObject> {
        // 1. Let proto be ? targetFunction.[[GetPrototypeOf]]().
        let proto = target_function.__get_prototype_of__(context)?;
        let is_constructor = target_function.is_constructor();

        // 2. Let internalSlotsList be the internal slots listed in Table 35, plus [[Prototype]] and [[Extensible]].
        // 3. Let obj be ! MakeBasicObject(internalSlotsList).
        // 4. Set obj.[[Prototype]] to proto.
        // 5. Set obj.[[Call]] as described in 10.4.1.1.
        // 6. If IsConstructor(targetFunction) is true, then
        // a. Set obj.[[Construct]] as described in 10.4.1.2.
        // 7. Set obj.[[BoundTargetFunction]] to targetFunction.
        // 8. Set obj.[[BoundThis]] to boundThis.
        // 9. Set obj.[[BoundArguments]] to boundArgs.
        // 10. Return obj.
        Ok(JsObject::from_proto_and_data(
            proto,
            ObjectData::bound_function(
                BoundFunction {
                    target_function,
                    this,
                    args,
                },
                is_constructor,
            ),
        ))
    }

    /// Get a reference to the bound function's this.
    pub fn this(&self) -> &JsValue {
        &self.this
    }

    /// Get a reference to the bound function's target function.
    pub fn target_function(&self) -> &JsObject {
        &self.target_function
    }

    /// Get a reference to the bound function's args.
    pub fn args(&self) -> &[JsValue] {
        self.args.as_slice()
    }
}
