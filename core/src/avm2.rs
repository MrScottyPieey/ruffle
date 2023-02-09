//! ActionScript Virtual Machine 2 (AS3) support

use crate::avm2::class::AllocatorFn;
use crate::avm2::function::Executable;
use crate::avm2::globals::SystemClasses;
use crate::avm2::method::{Method, NativeMethodImpl};
use crate::avm2::script::{Script, TranslationUnit};
use crate::context::UpdateContext;
use crate::string::AvmString;
use fnv::FnvHashMap;
use gc_arena::{Collect, GcCell, MutationContext};
use swf::avm2::read::Reader;
use swf::{DoAbc, DoAbcFlag};

#[macro_export]
macro_rules! avm_debug {
    ($avm: expr, $($arg:tt)*) => (
        if $avm.show_debug_output() {
            tracing::debug!($($arg)*)
        }
    )
}

pub mod activation;
mod amf;
mod array;
pub mod bytearray;
mod call_stack;
mod class;
mod domain;
pub mod error;
mod events;
mod function;
pub mod globals;
mod method;
mod multiname;
mod namespace;
pub mod object;
mod property;
mod property_map;
mod qname;
mod regexp;
mod scope;
mod script;
mod string;
mod stubs;
mod traits;
mod value;
mod vector;
mod vtable;

pub use crate::avm2::activation::Activation;
pub use crate::avm2::array::ArrayStorage;
pub use crate::avm2::call_stack::{CallNode, CallStack};
pub use crate::avm2::domain::Domain;
pub use crate::avm2::error::Error;
pub use crate::avm2::globals::flash::ui::context_menu::make_context_menu_state;
pub use crate::avm2::multiname::Multiname;
pub use crate::avm2::namespace::{Namespace, NamespaceData};
pub use crate::avm2::object::{
    ArrayObject, ClassObject, EventObject, Object, ScriptObject, SoundChannelObject, StageObject,
    TObject,
};
pub use crate::avm2::qname::QName;
pub use crate::avm2::value::Value;

use self::scope::Scope;

const BROADCAST_WHITELIST: [&str; 4] = ["enterFrame", "exitFrame", "frameConstructed", "render"];

/// The state of an AVM2 interpreter.
#[derive(Collect)]
#[collect(no_drop)]
pub struct Avm2<'gc> {
    /// Values currently present on the operand stack.
    stack: Vec<Value<'gc>>,

    /// Scopes currently present of the scope stack.
    scope_stack: Vec<Scope<'gc>>,

    /// The current call stack of the player.
    call_stack: GcCell<'gc, CallStack<'gc>>,

    /// Global scope object.
    globals: Domain<'gc>,

    /// System classes.
    system_classes: Option<SystemClasses<'gc>>,

    pub public_namespace: Namespace<'gc>,
    pub as3_namespace: Namespace<'gc>,
    pub vector_public_namespace: Namespace<'gc>,
    pub vector_internal_namespace: Namespace<'gc>,
    pub proxy_namespace: Namespace<'gc>,
    pub ruffle_private_namespace: Namespace<'gc>,
    // these are required to facilitate shared access between Rust and AS
    pub flash_display_internal: Namespace<'gc>,
    pub flash_utils_internal: Namespace<'gc>,
    pub flash_geom_internal: Namespace<'gc>,

    #[collect(require_static)]
    native_method_table: &'static [Option<(&'static str, NativeMethodImpl)>],

    #[collect(require_static)]
    native_instance_allocator_table: &'static [Option<(&'static str, AllocatorFn)>],

    #[collect(require_static)]
    native_instance_init_table: &'static [Option<(&'static str, NativeMethodImpl)>],

    /// A list of objects which are capable of recieving broadcasts.
    ///
    /// Certain types of events are "broadcast events" that are emitted on all
    /// constructed objects in order of their creation, whether or not they are
    /// currently present on the display list. This list keeps track of that.
    ///
    /// TODO: These should be weak object pointers, but our current garbage
    /// collector does not support weak references.
    broadcast_list: FnvHashMap<AvmString<'gc>, Vec<Object<'gc>>>,

    #[cfg(feature = "avm_debug")]
    pub debug_output: bool,
}

impl<'gc> Avm2<'gc> {
    /// Construct a new AVM interpreter.
    pub fn new(mc: MutationContext<'gc, '_>) -> Self {
        let globals = Domain::global_domain(mc);

        Self {
            stack: Vec::new(),
            scope_stack: Vec::new(),
            call_stack: GcCell::allocate(mc, CallStack::new()),
            globals,
            system_classes: None,

            public_namespace: Namespace::package("", mc),
            as3_namespace: Namespace::package("http://adobe.com/AS3/2006/builtin", mc),
            vector_public_namespace: Namespace::package("__AS3__.vec", mc),
            vector_internal_namespace: Namespace::internal("__AS3__.vec", mc),
            proxy_namespace: Namespace::package(
                "http://www.adobe.com/2006/actionscript/flash/proxy",
                mc,
            ),
            ruffle_private_namespace: Namespace::private("", mc),
            // these are required to facilitate shared access between Rust and AS
            flash_display_internal: Namespace::internal("flash.display", mc),
            flash_utils_internal: Namespace::internal("flash.utils", mc),
            flash_geom_internal: Namespace::internal("flash.geom", mc),

            native_method_table: Default::default(),
            native_instance_allocator_table: Default::default(),
            native_instance_init_table: Default::default(),
            broadcast_list: Default::default(),

            #[cfg(feature = "avm_debug")]
            debug_output: false,
        }
    }

    pub fn load_player_globals(context: &mut UpdateContext<'_, 'gc>) -> Result<(), Error<'gc>> {
        let globals = context.avm2.globals;
        let mut activation = Activation::from_nothing(context.reborrow());
        globals::load_player_globals(&mut activation, globals)
    }

    /// Return the current set of system classes.
    ///
    /// This function panics if the interpreter has not yet been initialized.
    pub fn classes(&self) -> &SystemClasses<'gc> {
        self.system_classes.as_ref().unwrap()
    }

    /// Run a script's initializer method.
    pub fn run_script_initializer(
        script: Script<'gc>,
        context: &mut UpdateContext<'_, 'gc>,
    ) -> Result<(), Error<'gc>> {
        let mut init_activation = Activation::from_script(context.reborrow(), script)?;

        let (method, scope, _domain) = script.init();
        match method {
            Method::Native(method) => {
                //This exists purely to check if the builtin is OK with being called with
                //no parameters.
                init_activation.resolve_parameters(&method.name, &[], &method.signature)?;
                init_activation
                    .context
                    .avm2
                    .push_global_init(init_activation.context.gc_context);
                let r = (method.method)(&mut init_activation, Some(scope), &[]);
                init_activation
                    .context
                    .avm2
                    .pop_call(init_activation.context.gc_context);
                r?;
            }
            Method::Bytecode(method) => {
                init_activation
                    .context
                    .avm2
                    .push_global_init(init_activation.context.gc_context);
                let r = init_activation.run_actions(method);
                init_activation
                    .context
                    .avm2
                    .pop_call(init_activation.context.gc_context);
                r?;
            }
        };

        Ok(())
    }

    /// Dispatch an event on an object.
    ///
    /// The `bool` parameter reads true if the event was cancelled.
    pub fn dispatch_event(
        context: &mut UpdateContext<'_, 'gc>,
        event: Object<'gc>,
        target: Object<'gc>,
    ) -> Result<bool, Error<'gc>> {
        use crate::avm2::events::dispatch_event;
        let mut activation = Activation::from_nothing(context.reborrow());
        dispatch_event(&mut activation, target, event)
    }

    /// Add an object to the broadcast list.
    ///
    /// Each broadcastable event contains it's own broadcast list. You must
    /// register all objects that have event handlers with that event's
    /// broadcast list by calling this function. Attempting to register a
    /// broadcast listener for a non-broadcast event will do nothing.
    ///
    /// Attempts to register the same listener for the same event will also do
    /// nothing.
    pub fn register_broadcast_listener(
        context: &mut UpdateContext<'_, 'gc>,
        object: Object<'gc>,
        event_name: AvmString<'gc>,
    ) {
        if !BROADCAST_WHITELIST
            .iter()
            .any(|x| AvmString::from(*x) == event_name)
        {
            return;
        }

        let bucket = context.avm2.broadcast_list.entry(event_name).or_default();

        if bucket.iter().any(|x| Object::ptr_eq(*x, object)) {
            return;
        }

        bucket.push(object);
    }

    /// Dispatch an event on all objects in the current execution list.
    ///
    /// `on_type` specifies a class or interface constructor whose instances,
    /// implementers, and/or subclasses define the set of objects that will
    /// receive the event. You can broadcast to just display objects, or
    /// specific interfaces, and so on.
    ///
    /// Attempts to broadcast a non-broadcast event will do nothing. To add a
    /// new broadcast type, you must add it to the `BROADCAST_WHITELIST` first.
    pub fn broadcast_event(
        context: &mut UpdateContext<'_, 'gc>,
        event: Object<'gc>,
        on_type: ClassObject<'gc>,
    ) -> Result<(), Error<'gc>> {
        let base_event = event.as_event().unwrap(); // TODO: unwrap?
        let event_name = base_event.event_type();
        drop(base_event);
        if !BROADCAST_WHITELIST
            .iter()
            .any(|x| AvmString::from(*x) == event_name)
        {
            return Ok(());
        }

        let el_length = context
            .avm2
            .broadcast_list
            .entry(event_name)
            .or_default()
            .len();

        for i in 0..el_length {
            let object = context
                .avm2
                .broadcast_list
                .get(&event_name)
                .unwrap()
                .get(i)
                .copied();

            if let Some(object) = object {
                let mut activation = Activation::from_nothing(context.reborrow());

                if object.is_of_type(on_type, &mut activation) {
                    Avm2::dispatch_event(&mut activation.context, event, object)?;
                }
            }
        }

        Ok(())
    }

    pub fn run_stack_frame_for_callable(
        callable: Object<'gc>,
        reciever: Option<Object<'gc>>,
        args: &[Value<'gc>],
        context: &mut UpdateContext<'_, 'gc>,
    ) -> Result<(), Error<'gc>> {
        let mut evt_activation = Activation::from_nothing(context.reborrow());
        callable.call(reciever, args, &mut evt_activation)?;

        Ok(())
    }

    /// Load an ABC file embedded in a `DoAbc` tag.
    pub fn do_abc(
        context: &mut UpdateContext<'_, 'gc>,
        do_abc: DoAbc,
        domain: Domain<'gc>,
    ) -> Result<(), Error<'gc>> {
        let mut reader = Reader::new(do_abc.data);
        let abc = match reader.read() {
            Ok(abc) => abc,
            Err(_) => {
                let mut activation = Activation::from_nothing(context.reborrow());
                return Err(Error::AvmError(crate::avm2::error::verify_error(
                    &mut activation,
                    "Error #1107: The ABC data is corrupt, attempt to read out of bounds.",
                    1107,
                )?));
            }
        };

        let num_scripts = abc.scripts.len();
        let tunit = TranslationUnit::from_abc(abc, domain, context.gc_context);
        for i in (0..num_scripts).rev() {
            tunit.load_script(i as u32, context)?;
        }

        if !do_abc.flags.contains(DoAbcFlag::LAZY_INITIALIZE) {
            for i in 0..num_scripts {
                if let Some(mut script) = tunit.get_script(i) {
                    script.globals(context)?;
                }
            }
        }
        Ok(())
    }

    pub fn global_domain(&self) -> Domain<'gc> {
        self.globals
    }

    /// Pushes an executable on the call stack
    pub fn push_call(&self, mc: MutationContext<'gc, '_>, calling: Executable<'gc>) {
        self.call_stack.write(mc).push(calling)
    }

    /// Pushes script initializer (global init) on the call stack
    pub fn push_global_init(&self, mc: MutationContext<'gc, '_>) {
        self.call_stack.write(mc).push_global_init()
    }

    /// Pops an executable off the call stack
    pub fn pop_call(&self, mc: MutationContext<'gc, '_>) -> Option<CallNode<'gc>> {
        self.call_stack.write(mc).pop()
    }

    pub fn call_stack(&self) -> GcCell<'gc, CallStack<'gc>> {
        self.call_stack
    }

    /// Push a value onto the operand stack.
    fn push(&mut self, value: impl Into<Value<'gc>>, depth: usize, max: usize) {
        if self.stack.len() - depth > max {
            tracing::warn!("Avm2::push: Stack overflow");
            return;
        }
        let mut value = value.into();
        if let Value::Object(o) = value {
            if let Some(prim) = o.as_primitive() {
                value = *prim;
            }
        }

        avm_debug!(self, "Stack push {}: {value:?}", self.stack.len());
        self.stack.push(value);
    }

    /// Retrieve the top-most value on the operand stack.
    #[allow(clippy::let_and_return)]
    fn pop(&mut self, depth: usize) -> Value<'gc> {
        let value = if self.stack.len() <= depth {
            tracing::warn!("Avm2::pop: Stack underflow");
            Value::Undefined
        } else {
            self.stack.pop().unwrap_or(Value::Undefined)
        };

        avm_debug!(self, "Stack pop {}: {value:?}", self.stack.len());

        value
    }

    /// Peek the n-th value from the end of the operand stack.
    #[allow(clippy::let_and_return)]
    fn peek(&mut self, index: usize) -> Value<'gc> {
        let value = self
            .stack
            .get(self.stack.len() - index - 1)
            .copied()
            .unwrap_or_else(|| {
                tracing::warn!("Avm1::pop: Stack underflow");
                Value::Undefined
            });

        avm_debug!(self, "Stack peek {}: {value:?}", self.stack.len());

        value
    }

    fn pop_args(&mut self, arg_count: u32, depth: usize) -> Vec<Value<'gc>> {
        let mut args = vec![Value::Undefined; arg_count as usize];
        for arg in args.iter_mut().rev() {
            *arg = self.pop(depth);
        }
        args
    }

    fn push_scope(&mut self, scope: Scope<'gc>, depth: usize, max: usize) {
        if self.scope_stack.len() - depth > max {
            tracing::warn!("Avm2::push_scope: Scope Stack overflow");
            return;
        }

        self.scope_stack.push(scope);
    }

    fn pop_scope(&mut self, depth: usize) -> Option<Scope<'gc>> {
        if self.scope_stack.len() <= depth {
            tracing::warn!("Avm2::pop_scope: Scope Stack underflow");
            None
        } else {
            self.scope_stack.pop()
        }
    }

    #[cfg(feature = "avm_debug")]
    #[inline]
    pub fn show_debug_output(&self) -> bool {
        self.debug_output
    }

    #[cfg(not(feature = "avm_debug"))]
    pub const fn show_debug_output(&self) -> bool {
        false
    }

    #[cfg(feature = "avm_debug")]
    pub fn set_show_debug_output(&mut self, visible: bool) {
        self.debug_output = visible;
    }

    #[cfg(not(feature = "avm_debug"))]
    pub const fn set_show_debug_output(&self, _visible: bool) {}
}
