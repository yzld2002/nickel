use std::{
    any::type_name, cell::Cell, marker::PhantomData, mem, ops::Deref, ptr::NonNull,
    sync::atomic::Ordering::Relaxed,
};

use crate::{
    blocks::Header,
    gc::{self, Gc},
    internals::{
        self,
        gc_stats::{BLOCK_COUNT, POST_BLOCK_COUNT},
    },
    AsStatic, GcInfo, GC,
};

#[derive(Clone)]
pub struct RootGc<T: 'static + GC> {
    pub(crate) root: Root,
    _data: PhantomData<T>,
}

impl<T: GC + AsStatic> RootGc<T>
where
    T::Static: GC,
{
    pub fn from_gc(gc: Gc<T>) -> RootGc<T::Static> {
        unsafe { mem::transmute(Root::from_gc(gc)) }
    }
}

/// This impl is here to help migrate.
/// It's not less safe than the rest of the API currently, but it cannot ever be made fully safe.
impl<T: GC> Deref for RootGc<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*((self.root.inner.as_ref()).ptr.get() as *const T) }
    }
}

#[derive(PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Root {
    /// Constructing a Root is unsafe.
    /// FIXME make private
    pub(crate) inner: NonNull<RootInner>,
}

impl Root {
    /// This `Root::from_gc` should be preferred over the `From` impl to aid with inference.
    pub fn from_gc<T: GC>(gc: Gc<T>) -> Root {
        let roots = unsafe { &mut *Header::from_gc(gc).evaced.get() };
        let obj_status = roots
            .entry(gc.0 as *const T as *const u8)
            .or_insert_with(|| {
                ObjectStatus::Rooted(NonNull::from(Box::leak(Box::new(RootInner::new(gc)))))
            });

        let inner = match obj_status {
            ObjectStatus::Rooted(r) => *r,
            e => panic!("Attempted to root a object with existing status: {:?}", e),
        };

        Root { inner }
    }

    /// This is horribly unsafe!!!
    /// It only exists because migrating from `Rc<T>` to `Gc<'static, T>`
    /// is much simpler than migrating to the safe `Gc<'generation, T>` API.
    ///
    /// # Safety
    ///
    /// This function runs destructors and deallocates memory.
    /// Improper usage will result in use after frees,
    /// segfaults, and every other bad thing you can think of.
    ///
    /// By using this function you must guarantee:
    /// 1. No `Gc<T>`'s exist on this thread, unless they transitively pointed to by a `Root`.
    /// 2. No references to any `Gc`s or their contents exist in this thread.
    pub unsafe fn collect_garbage() {
        if BLOCK_COUNT.load(Relaxed) >= (2 * POST_BLOCK_COUNT.load(Relaxed)) {
            internals::run_evac()
        }
    }
}

unsafe impl GC for Root {
    unsafe fn trace(s: &Self, direct_gc_ptrs: *mut Vec<()>) {
        let inner = s.inner.as_ref();
        let ptr = inner.ptr.get();

        let traced_count = if inner.collection_marker.get() == internals::marker() {
            let traced_count = inner.traced_count.get();
            inner.traced_count.set(traced_count + 1);
            traced_count
        } else {
            inner.collection_marker.set(internals::marker());
            inner.traced_count.set(1);
            1
        };

        let ref_count = inner.ref_count.get();
        if traced_count == ref_count {
            // All `Root`s live in the GC heap.
            // Hence we can now demote them to a ordinary `Gc`
            let header = &*Header::from_ptr(ptr as usize);
            let evaced = &mut *header.evaced.get();
            evaced.remove(&ptr);
        };
        let direct_gc_ptrs = mem::transmute::<_, *mut Vec<TraceAt>>(direct_gc_ptrs);
        (inner.trace_fn)(ptr as *mut _, direct_gc_ptrs)
    }
    const SAFE_TO_DROP: bool = true;
}

impl Clone for Root {
    fn clone(&self) -> Self {
        let inner = unsafe { self.inner.as_ref() };
        let ref_count = inner.ref_count.get();
        inner.ref_count.set(ref_count + 1);

        Root { inner: self.inner }
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let inner = unsafe { self.inner.as_ref() };
        let ref_count = inner.ref_count.get();
        inner.ref_count.set(ref_count - 1);
        // Running destructors is handled by the Underlying Gc, not Root.
        // TODO add debug assertions
    }
}

impl<'r, T: GC> From<gc::Gc<'r, T>> for Root {
    fn from(gc: gc::Gc<'r, T>) -> Self {
        Root::from_gc(gc)
    }
}

impl<T: 'static + GC> From<RootGc<T>> for Root {
    fn from(root: RootGc<T>) -> Self {
        root.root
    }
}

impl<T: 'static + GC> TryFrom<Root> for RootGc<T> {
    type Error = String;

    fn try_from(root: Root) -> Result<Self, Self::Error> {
        let ptr = unsafe { root.inner.as_ref() }.ptr.get();
        let header = unsafe { &*Header::from_ptr(ptr as usize) };
        if header.info == GcInfo::of::<T>() {
            Ok(RootGc {
                root,
                _data: PhantomData,
            })
        } else {
            Err(format!(
                "The Root is of type:          `{:?}`\nyou tried to convert it to a: `{}`",
                header,
                type_name::<T>()
            ))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TraceAt {
    /// A `*const Gc<T>`.
    pub ptr_to_gc: *const *const u8,
    pub trace_fn: fn(*mut u8, *mut Vec<TraceAt>),
}
impl TraceAt {
    pub fn of_val<T: GC>(t: &Gc<T>) -> Self {
        TraceAt {
            ptr_to_gc: t as *const Gc<T> as *const *const u8,
            trace_fn: unsafe { std::mem::transmute(T::trace as usize) },
        }
    }
}

/// It's safe to use `RootAt` as a key,
/// since it's impls ignore it's mutable field `ptr: AtomicUsize`.
/// E.g. `#[allow(clippy::mutable_key_type)]`
///
/// This is like a Rc, but it handles cycles.
///
/// TODO make !Send, and !Sync
/// See if UnsafeCell is any faster.
/// For now I'm using Atomics with Relaxed ordering because it's simpler.
#[derive(Debug)]
pub struct RootInner {
    /// `ptr` is a `*const T`
    pub(crate) ptr: Cell<*const u8>,
    pub(crate) trace_fn: fn(*mut u8, *mut Vec<TraceAt>),
    // drop_fn: unsafe fn(*mut u8),
    /// The marker of the collection phase asscoated with the traced_count.
    /// Right now it's just a two space collector, hence bool.
    collection_marker: Cell<bool>,
    /// The number of references evacuated durring a collection phase.
    traced_count: Cell<usize>,
    /// This is the count of all owning references.
    /// ref_count >= traced_count
    ref_count: Cell<usize>,
}

impl RootInner {
    fn new<T: GC>(t: crate::gc::Gc<T>) -> Self {
        let obj_ptr = t.0 as *const T;
        // dbg!(obj_ptr);
        let header = Header::from_ptr(obj_ptr as usize);
        Header::checksum(header);

        RootInner {
            ptr: Cell::from(obj_ptr as *const u8),
            trace_fn: unsafe { std::mem::transmute(T::trace as usize) },
            // drop_fn: unsafe { mem::transmute(ptr::drop_in_place::<T> as usize) },
            collection_marker: Cell::from(internals::marker()),
            traced_count: Cell::from(0),
            ref_count: Cell::from(1),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
pub enum ObjectStatus {
    /// The object was moved to the pointer.
    Moved(*const u8),
    /// The object is rooted.
    /// `RootInner.ptr` always points to the current location of the object.
    /// If `RootInner.ptr` is in this `Block` the object has yet to be evacuated.
    Rooted(NonNull<RootInner>),
    /// The object's destructor has been run.
    /// This is only needed for types that are not marked safe to drop.
    Dropped,
}
