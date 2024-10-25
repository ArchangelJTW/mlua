use std::cell::RefCell;
use std::marker::PhantomData;
use std::mem;
use std::os::raw::c_void;

use crate::error::{Error, Result};
use crate::function::Function;
use crate::state::{Lua, LuaGuard, RawLua};
use crate::traits::{FromLuaMulti, IntoLuaMulti};
use crate::types::{Callback, CallbackUpvalue, ScopedCallback, ValueRef};
use crate::userdata::{AnyUserData, UserData, UserDataRegistry, UserDataStorage};
use crate::util::{
    self, assert_stack, check_stack, get_metatable_ptr, get_userdata, take_userdata, StackGuard,
};

/// Constructed by the [`Lua::scope`] method, allows temporarily creating Lua userdata and
/// callbacks that are not required to be `Send` or `'static`.
///
/// See [`Lua::scope`] for more details.
pub struct Scope<'scope, 'env: 'scope> {
    lua: LuaGuard,
    destructors: Destructors<'env>,
    _scope_invariant: PhantomData<&'scope mut &'scope ()>,
    _env_invariant: PhantomData<&'env mut &'env ()>,
}

type DestructorCallback<'a> = Box<dyn FnOnce(&RawLua, ValueRef) -> Vec<Box<dyn FnOnce() + 'a>>>;

// Implement Drop on Destructors instead of Scope to avoid compilation error
struct Destructors<'a>(RefCell<Vec<(ValueRef, DestructorCallback<'a>)>>);

impl<'scope, 'env: 'scope> Scope<'scope, 'env> {
    pub(crate) fn new(lua: LuaGuard) -> Self {
        Scope {
            lua,
            destructors: Destructors(RefCell::new(Vec::new())),
            _scope_invariant: PhantomData,
            _env_invariant: PhantomData,
        }
    }

    /// Wraps a Rust function or closure, creating a callable Lua function handle to it.
    ///
    /// This is a version of [`Lua::create_function`] that creates a callback which expires on
    /// scope drop. See [`Lua::scope`] for more details.
    pub fn create_function<F, A, R>(&'scope self, func: F) -> Result<Function>
    where
        F: Fn(&Lua, A) -> Result<R> + 'scope,
        A: FromLuaMulti,
        R: IntoLuaMulti,
    {
        unsafe {
            self.create_callback(Box::new(move |rawlua, nargs| {
                let args = A::from_stack_args(nargs, 1, None, rawlua)?;
                func(rawlua.lua(), args)?.push_into_stack_multi(rawlua)
            }))
        }
    }

    /// Wraps a Rust mutable closure, creating a callable Lua function handle to it.
    ///
    /// This is a version of [`Lua::create_function_mut`] that creates a callback which expires
    /// on scope drop. See [`Lua::scope`] and [`Scope::create_function`] for more details.
    pub fn create_function_mut<F, A, R>(&'scope self, func: F) -> Result<Function>
    where
        F: FnMut(&Lua, A) -> Result<R> + 'scope,
        A: FromLuaMulti,
        R: IntoLuaMulti,
    {
        let func = RefCell::new(func);
        self.create_function(move |lua, args| {
            (*func.try_borrow_mut().map_err(|_| Error::RecursiveMutCallback)?)(lua, args)
        })
    }

    /// Creates a Lua userdata object from a reference to custom userdata type.
    ///
    /// This is a version of [`Lua::create_userdata`] that creates a userdata which expires on
    /// scope drop, and does not require that the userdata type be Send. This method takes
    /// non-'static reference to the data. See [`Lua::scope`] for more details.
    ///
    /// Userdata created with this method will not be able to be mutated from Lua.
    pub fn create_userdata_ref<T>(&'scope self, data: &'env T) -> Result<AnyUserData>
    where
        T: UserData + 'static,
    {
        unsafe {
            let ud = self.lua.make_userdata(UserDataStorage::new_ref(data))?;
            self.seal_userdata::<T>(&ud)?;
            Ok(ud)
        }
    }

    /// Creates a Lua userdata object from a mutable reference to custom userdata type.
    ///
    /// This is a version of [`Lua::create_userdata`] that creates a userdata which expires on
    /// scope drop, and does not require that the userdata type be Send. This method takes
    /// non-'static mutable reference to the data. See [`Lua::scope`] for more details.
    pub fn create_userdata_ref_mut<T>(&'scope self, data: &'env mut T) -> Result<AnyUserData>
    where
        T: UserData + 'static,
    {
        unsafe {
            let ud = self.lua.make_userdata(UserDataStorage::new_ref_mut(data))?;
            self.seal_userdata::<T>(&ud)?;
            Ok(ud)
        }
    }

    /// Creates a Lua userdata object from a reference to custom Rust type.
    ///
    /// This is a version of [`Lua::create_any_userdata`] that creates a userdata which expires on
    /// scope drop, and does not require that the Rust type be Send. This method takes non-'static
    /// reference to the data. See [`Lua::scope`] for more details.
    ///
    /// Userdata created with this method will not be able to be mutated from Lua.
    pub fn create_any_userdata_ref<T>(&'scope self, data: &'env T) -> Result<AnyUserData>
    where
        T: 'static,
    {
        unsafe {
            let ud = self.lua.make_any_userdata(UserDataStorage::new_ref(data))?;
            self.seal_userdata::<T>(&ud)?;
            Ok(ud)
        }
    }

    /// Creates a Lua userdata object from a mutable reference to custom Rust type.
    ///
    /// This is a version of [`Lua::create_any_userdata`] that creates a userdata which expires on
    /// scope drop, and does not require that the Rust type be Send. This method takes non-'static
    /// mutable reference to the data. See [`Lua::scope`] for more details.
    pub fn create_any_userdata_ref_mut<T>(&'scope self, data: &'env mut T) -> Result<AnyUserData>
    where
        T: 'static,
    {
        unsafe {
            let ud = self.lua.make_any_userdata(UserDataStorage::new_ref_mut(data))?;
            self.seal_userdata::<T>(&ud)?;
            Ok(ud)
        }
    }

    /// Creates a Lua userdata object from a custom userdata type.
    ///
    /// This is a version of [`Lua::create_userdata`] that creates a userdata which expires on
    /// scope drop, and does not require that the userdata type be `Send` or `'static`. See
    /// [`Lua::scope`] for more details.
    ///
    /// The main limitation that comes from using non-'static userdata is that the produced userdata
    /// will no longer have a [`TypeId`] associated with it, because [`TypeId`] can only work for
    /// `'static` types. This means that it is impossible, once the userdata is created, to get a
    /// reference to it back *out* of an [`AnyUserData`] handle. This also implies that the
    /// "function" type methods that can be added via [`UserDataMethods`] (the ones that accept
    /// [`AnyUserData`] as a first parameter) are vastly less useful. Also, there is no way to
    /// re-use a single metatable for multiple non-'static types, so there is a higher cost
    /// associated with creating the userdata metatable each time a new userdata is created.
    ///
    /// [`TypeId`]: std::any::TypeId
    /// [`UserDataMethods`]: crate::UserDataMethods
    pub fn create_userdata<T>(&'scope self, data: T) -> Result<AnyUserData>
    where
        T: UserData + 'env,
    {
        let state = self.lua.state();
        unsafe {
            let _sg = StackGuard::new(state);
            check_stack(state, 3)?;

            // // We don't write the data to the userdata until pushing the metatable
            let protect = !self.lua.unlikely_memory_error();
            #[cfg(feature = "luau")]
            let ud_ptr = {
                let data = UserDataStorage::new_scoped(data);
                util::push_userdata::<UserDataStorage<T>>(state, data, protect)?
            };
            #[cfg(not(feature = "luau"))]
            let ud_ptr = util::push_uninit_userdata::<UserDataStorage<T>>(state, protect)?;

            // Push the metatable and register it with no TypeId
            let mut registry = UserDataRegistry::new_unique(ud_ptr as *const c_void);
            T::register(&mut registry);
            self.lua.push_userdata_metatable(registry)?;
            let mt_ptr = ffi::lua_topointer(state, -1);
            self.lua.register_userdata_metatable(mt_ptr, None);

            // Write data to the pointer and attach metatable
            #[cfg(not(feature = "luau"))]
            std::ptr::write(ud_ptr, UserDataStorage::new_scoped(data));
            ffi::lua_setmetatable(state, -2);

            let ud = AnyUserData(self.lua.pop_ref());

            let destructor: DestructorCallback = Box::new(|rawlua, vref| {
                let state = rawlua.state();
                let _sg = StackGuard::new(state);
                assert_stack(state, 2);

                // Check that userdata is valid (very likely)
                if rawlua.push_userdata_ref(&vref).is_err() {
                    return vec![];
                }

                // Deregister metatable
                let mt_ptr = get_metatable_ptr(state, -1);
                rawlua.deregister_userdata_metatable(mt_ptr);

                let ud = take_userdata::<UserDataStorage<T>>(state);

                vec![Box::new(move || drop(ud))]
            });
            self.destructors.0.borrow_mut().push((ud.0.clone(), destructor));

            Ok(ud)
        }
    }

    unsafe fn create_callback(&'scope self, f: ScopedCallback<'scope>) -> Result<Function> {
        let f = mem::transmute::<ScopedCallback, Callback>(f);
        let f = self.lua.create_callback(f)?;

        let destructor: DestructorCallback = Box::new(|rawlua, vref| {
            let ref_thread = rawlua.ref_thread();
            ffi::lua_getupvalue(ref_thread, vref.index, 1);
            let upvalue = get_userdata::<CallbackUpvalue>(ref_thread, -1);
            let data = (*upvalue).data.take();
            ffi::lua_pop(ref_thread, 1);
            vec![Box::new(move || drop(data))]
        });
        self.destructors.0.borrow_mut().push((f.0.clone(), destructor));

        Ok(f)
    }

    /// Shortens the lifetime of the userdata to the lifetime of the scope.
    unsafe fn seal_userdata<T: 'static>(&self, ud: &AnyUserData) -> Result<()> {
        let destructor: DestructorCallback = Box::new(|rawlua, vref| {
            let state = rawlua.state();
            let _sg = StackGuard::new(state);
            assert_stack(state, 2);

            // Ensure that userdata is not destructed
            if rawlua.push_userdata_ref(&vref).is_err() {
                return vec![];
            }

            let data = take_userdata::<UserDataStorage<T>>(state);
            vec![Box::new(move || drop(data))]
        });
        self.destructors.0.borrow_mut().push((ud.0.clone(), destructor));

        Ok(())
    }
}

impl Drop for Destructors<'_> {
    fn drop(&mut self) {
        // We separate the action of invalidating the userdata in Lua and actually dropping the
        // userdata type into two phases. This is so that, in the event a userdata drop panics,
        // we can be sure that all of the userdata in Lua is actually invalidated.

        let destructors = mem::take(&mut *self.0.borrow_mut());
        if let Some(lua) = destructors.first().map(|(vref, _)| vref.lua.lock()) {
            // All destructors are non-panicking, so this is fine
            let to_drop = destructors
                .into_iter()
                .flat_map(|(vref, destructor)| destructor(&lua, vref))
                .collect::<Vec<_>>();

            drop(to_drop);
        }
    }
}
