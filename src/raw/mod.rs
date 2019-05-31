use std::borrow::Borrow;
use std::hash::{BuildHasher, Hash, Hasher};
use std::marker::PhantomData;
use std::mem;
use std::ops::Deref;
use std::sync::atomic::Ordering;

use arrayvec::ArrayVec;
use bitflags::bitflags;
use crossbeam_epoch::{Atomic, Guard, Owned, Shared};
use smallvec::SmallVec;

pub mod config;

use self::config::Config;

// TODO: Rename Leaf. It's a public name and a bit silly/leaks implementation details.
// TODO: Make this whole type private and implement better/all APIs around it? Maybe make it
// customizable even more ‒ synchronization, keys other than hashes (arbitrary byte strings?),
// copy/clone directly instead of storing just the key. But certainly different things for sets (we
// want the whole API to be Arc<K>, not Arc<Node>).
// TODO: Iterators (from, into, extend)
// TODO: Rayon support (from and into parallel iterator, extend) under a feature flag.
// TODO: Valgrind into the CI
// TODO: Split into multiple files
// TODO: Some refactoring around the pointer juggling. This seems to be error prone.

// All directly written, some things are not const fn yet :-(. But tested below.
pub(crate) const LEVEL_BITS: usize = 4;
pub(crate) const LEVEL_MASK: u64 = 0b1111;
pub(crate) const LEVEL_CELLS: usize = 16;
pub(crate) const MAX_LEVELS: usize = mem::size_of::<u64>() * 8 / LEVEL_BITS;

// TODO: Checks that we really do have the bits in the alignment.
bitflags! {
    /// Flags that can be put onto a pointer pointing to a node, specifying some interesting
    /// things.
    ///
    /// Note that this lives inside the unused bits of a pointer. All nodes align at least to a
    /// machine word and we assume it's at least 32bits, so we have at least 2 bits.
    struct NodeFlags: usize {
        /// The Inner containing this pointer is condemned to replacement/pruning.
        ///
        /// Changing this pointer is pointer is forbidden, and the containing Inner needs to be
        /// replaced first with a clean one.
        const CONDEMNED = 0b01;
        /// The pointer points not to an inner node, but to data node.
        ///
        /// TODO: Describe the trick better.
        const DATA = 0b10;
    }
}

fn nf(node: Shared<Inner>) -> NodeFlags {
    NodeFlags::from_bits(node.tag()).expect("Invalid node flags")
}

unsafe fn load_data<'a, C: Config>(node: Shared<'a, Inner>) -> &'a Data<C> {
    assert!(
        nf(node).contains(NodeFlags::DATA),
        "Tried to load data from inner node pointer"
    );
    (node.as_raw() as usize as *const Data<C>)
        .as_ref()
        .expect("A null pointer with data flag found")
}

fn owned_data<C: Config>(data: Data<C>) -> Owned<Inner> {
    unsafe {
        Owned::<Inner>::from_raw(Box::into_raw(Box::new(data)) as usize as *mut _)
            .with_tag(NodeFlags::DATA.bits())
    }
}

unsafe fn drop_data<C: Config>(ptr: Shared<Inner>) {
    drop(Owned::from_raw(ptr.as_raw() as usize as *mut Data<C>));
}

#[derive(Default)]
struct Inner([Atomic<Inner>; LEVEL_CELLS]);

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct Leaf<K, V> {
    data: (K, V),
}

impl<K, V> Leaf<K, V> {
    pub fn new(key: K, value: V) -> Self {
        Self { data: (key, value) }
    }

    pub fn key(&self) -> &K {
        &self.data.0
    }

    pub fn value(&self) -> &V {
        &self.data.1
    }
}

impl<K, V> Deref for Leaf<K, V> {
    type Target = (K, V);
    fn deref(&self) -> &(K, V) {
        &self.data
    }
}

// Instead of distinguishing the very common case of single leaf and collision list in our code, we
// just handle everything as a list, possibly with 1 element.
//
// However, as the case with 1 element is much more probable, we don't want the Vec indirection
// there, so we let SmallVec to handle it by not spilling in that case. As the spilled Vec needs 2
// words in addition to the length (pointer and capacity), we have room for 2 Arcs in the not
// spilled case too, so we as well might take advantage of it.
// TODO: We want the union feature.
//
// Alternatively, we probably could use the raw allocator API and structure with len + [Arc<..>; 0].
// TODO: Compute the stack length based on the Payload size.
type Data<C> = SmallVec<[<C as Config>::Payload; 2]>;

enum TraverseState<C: Config, F> {
    Empty, // Invalid temporary state.
    Created(C::Payload),
    Future { key: C::Key, constructor: F },
}

impl<C: Config, F: FnOnce(C::Key) -> C::Payload> TraverseState<C, F> {
    fn key(&self) -> &C::Key {
        match self {
            TraverseState::Empty => unreachable!("Not supposed to live in the empty state"),
            TraverseState::Created(payload) => payload.borrow(),
            TraverseState::Future { key, .. } => key,
        }
    }
    fn payload(&mut self) -> C::Payload {
        let (new_val, result) = match mem::replace(self, TraverseState::Empty) {
            TraverseState::Empty => unreachable!("Not supposed to live in the empty state"),
            TraverseState::Created(payload) => (TraverseState::Created(payload.clone()), payload),
            TraverseState::Future { key, constructor } => {
                let payload = constructor(key);
                let created = TraverseState::Created(payload.clone());
                (created, payload)
            }
        };
        *self = new_val;
        result
    }
    fn data_owned(&mut self) -> Owned<Inner> {
        let mut data = Data::<C>::new();
        data.push(self.payload());
        owned_data::<C>(data)
    }
    fn into_payload(self) -> C::Payload {
        match self {
            TraverseState::Created(payload) => payload,
            TraverseState::Future { key, constructor } => constructor(key),
            TraverseState::Empty => unreachable!("Not supposed to live in the empty state"),
        }
    }
    fn into_return(self, mode: TraverseMode) -> Option<C::Payload> {
        match mode {
            TraverseMode::Overwrite => None,
            TraverseMode::IfMissing => Some(self.into_payload()),
        }
    }
}

#[derive(Copy, Clone, Eq, PartialEq)]
enum TraverseMode {
    Overwrite,
    IfMissing,
}

/// How well pruning went.
#[derive(Copy, Clone, Eq, PartialEq)]
enum PruneResult {
    /// Removed the node completely, inserted NULL into the parent.
    Null,
    /// Contracted an edge, inserted a lone child.
    Singleton,
    /// Made a copy, as there were multiple pointers leading from the child.
    Copy,
    /// Failed to update the parent, some other thread updated it in the meantime.
    CasFail,
}

pub struct Raw<C: Config, S> {
    hash_builder: S,
    root: Atomic<Inner>,
    _data: PhantomData<C::Payload>,
}

impl<C, S> Raw<C, S>
where
    C: Config,
    S: BuildHasher,
{
    pub fn with_hasher(hash_builder: S) -> Self {
        Self {
            hash_builder,
            root: Atomic::null(),
            _data: PhantomData,
        }
    }

    fn hash<Q>(&self, key: &Q) -> u64
    where
        Q: ?Sized + Hash,
    {
        let mut hasher = self.hash_builder.build_hasher();
        key.hash(&mut hasher);
        hasher.finish()
    }

    pub fn insert(&self, payload: C::Payload) -> Option<C::Payload> {
        self.traverse(
            // Any way to do it without the type parameters here? Older rustc doesn't like them.
            TraverseState::<C, fn(C::Key) -> C::Payload>::Created(payload),
            TraverseMode::Overwrite,
        )
    }

    /// Prunes the given node.
    ///
    /// * The parent points to the child node.
    /// * The child must be valid pointer, of course.
    ///
    /// The parent is made to point to either:
    /// * NULL if child is empty.
    /// * child's only child.
    /// * A copy of child.
    ///
    /// Returns how the pruning went.
    unsafe fn prune(pin: &Guard, parent: &Atomic<Inner>, child: Shared<Inner>) -> PruneResult {
        assert!(
            !nf(child).contains(NodeFlags::DATA),
            "Child passed to prune must not be data"
        );
        let inner = child.as_ref().expect("Null child node passed to prune");
        let mut allow_contract = true;
        let mut child_cnt = 0;
        let mut last_leaf = None;
        let mut new_child = Inner::default();

        // 1. Mark all the cells in this one as condemned.
        // 2. Look how many non-null branches are leading from there.
        // 3. Construct a copy of the child *without* the tags on the way.
        for (new, grandchild) in new_child.0.iter_mut().zip(&inner.0) {
            // Acquire ‒ we don't need the grandchild ourselves, only the pointer. But we'll need
            // to "republish" it through the parent pointer later on and for that we have to get it
            // first.
            //
            // FIXME: May we actually need SeqCst here to order it relative to the CAS below?
            let gc = grandchild.fetch_or(NodeFlags::CONDEMNED.bits(), Ordering::Acquire, pin);
            // The flags we insert into the new one should not contain condemned flag even if it
            // was already present here.
            let flags = nf(gc) & !NodeFlags::CONDEMNED;
            let gc = gc.with_tag(flags.bits());
            if gc.is_null() {
                // Do nothing, just skip
            } else if flags.contains(NodeFlags::DATA) {
                last_leaf.replace(gc);
                child_cnt += 1;
            } else {
                // If we have an inner node here, multiple leaves hang somewhere below there. More
                // importantly, we can't contrack the edge.
                allow_contract = false;
                child_cnt += 1;
            }

            *new = Atomic::from(gc);
        }

        // Now, decide what we want to put into the parent.
        let mut cleanup = None;
        let (insert, prune_result) = match (allow_contract, child_cnt, last_leaf) {
            // If there's exactly one leaf, we just contract the edge to lead there directly. Note
            // that we can't do that if this is not the leaf, because we would mess up the hash
            // matching on the way. But that's fine, we checked that above.
            (true, 1, Some(child)) => (child, PruneResult::Singleton),
            // If there's nothing, simply kill the node outright.
            (_, 0, None) => (Shared::null(), PruneResult::Null),
            // Many nodes (maybe somewhere below) ‒ someone must have inserted in between. But
            // we've already condemned this node, so create a new one and do the replacement.
            _ => {
                let new = Owned::new(new_child).into_shared(pin);
                // Note: we don't store Owned, because we may link it in. If we panicked before
                // disarming it, it would delete something linked in, which is bad. Instead, we
                // prefer deleting manually after the fact.
                cleanup = Some(new);
                (new, PruneResult::Copy)
            }
        };

        assert_eq!(
            0,
            child.tag(),
            "Attempt to replace condemned pointer or prune data node"
        );
        // Orderings: We need to publish the new node. We don't need to acquire the previous value
        // to destroy, because we already have it in case of success and we don't care about it on
        // failure.
        let result = parent
            .compare_and_set(child, insert, (Ordering::Release, Ordering::Relaxed), pin)
            .is_ok();
        if result {
            // We successfully unlinked the old child, so it's time to destroy it (as soon as
            // nobody is looking at it).
            pin.defer_destroy(child);
            prune_result
        } else {
            // We have failed to insert, so we need to clean up after ourselves.
            drop(cleanup.map(|c| Shared::into_owned(c)));
            PruneResult::CasFail
        }
    }

    fn traverse<F>(&self, mut state: TraverseState<C, F>, mode: TraverseMode) -> Option<C::Payload>
    where
        F: FnOnce(C::Key) -> C::Payload,
    {
        let hash = self.hash(state.key());
        let mut shift = 0;
        let mut current = &self.root;
        let mut parent = None;
        let pin = crossbeam_epoch::pin();
        loop {
            let node = current.load(Ordering::Acquire, &pin);
            let flags = nf(node);

            let replace = |with: Owned<Inner>, delete_previous| {
                // If we fail to set it, the `with` is dropped together with the Err case, freeing
                // whatever was inside it.
                let result = current.compare_and_set_weak(node, with, Ordering::Release, &pin);
                match result {
                    Ok(_) if !node.is_null() && delete_previous => {
                        assert!(flags.contains(NodeFlags::DATA));
                        let node = Shared::from(node.as_raw() as usize as *const Data<C>);
                        unsafe { pin.defer_destroy(node) };
                        true
                    }
                    Ok(_) => true,
                    Err(e) => {
                        if NodeFlags::from_bits(e.new.tag())
                            .expect("Invalid flags")
                            .contains(NodeFlags::DATA)
                        {
                            unsafe { drop_data::<C>(e.new.into_shared(&pin)) };
                        }
                        // Else → just let e drop and destroy the owned in there
                        false
                    }
                }
            };

            if flags.contains(NodeFlags::CONDEMNED) {
                // This one is going away. We are not allowed to modify the cell, we just have to
                // replace the inner node first. So, let's do some cleanup.
                //
                // TODO: In some cases we would not really *have* to do this (in particular, if we
                // just want to walk through and not modify it here at all, it's OK).
                unsafe {
                    let (parent, child) = parent.expect("Condemned the root!");
                    Self::prune(&pin, parent, child);
                }
                // Either us or someone else modified the tree on our path. In many cases we
                // could just continue here, but some cases are complex. For now, we just restart
                // the whole traversal and try from the start, for simplicity. This should be rare
                // anyway, so complicating the code further probably is not worth it.
                shift = 0;
                current = &self.root;
                parent = None;
            } else if node.is_null() {
                // Not found, create it.
                if replace(state.data_owned(), true) {
                    return state.into_return(mode);
                }
            // else -> retry
            } else if flags.contains(NodeFlags::DATA) {
                let data = unsafe { load_data::<C>(node) };
                if data.len() == 1
                    && data[0].borrow() != state.key()
                    && shift < mem::size_of_val(&hash) * 8
                {
                    // There's one data node at this pointer, but we want to place a different one
                    // here too. So we create a new level, push the old one down. Note that we
                    // check both that we are adding something else & that we still have some more
                    // bits to distinguish by.

                    // We need to add another level. Note: there *still* might be a collision.
                    // Therefore, we just add the level and try again.
                    let other_hash = self.hash(data[0].borrow());
                    let other_bits = (other_hash >> shift) & LEVEL_MASK;
                    let mut inner = Inner::default();
                    inner.0[other_bits as usize] = Atomic::from(node);
                    let split = Owned::new(inner);
                    // No matter if it succeeds or fails, we try again. We'll either find the newly
                    // inserted value here and continue with another level down, or it gets
                    // destroyed and we try splitting again.
                    replace(split, false);
                } else {
                    // All the other cases:
                    // * It has the same key
                    // * There's already a collision on this level (because we've already run out of
                    //   bits previously).
                    // * We've run out of the hash bits so there's nothing to split by any more.
                    let old = data
                        .iter()
                        .find(|l| (*l).borrow().borrow() == state.key())
                        .cloned();

                    if old.is_none() || mode == TraverseMode::Overwrite {
                        let mut new = Data::<C>::with_capacity(data.len() + 1);
                        new.extend(
                            data.iter()
                                .filter(|l| (*l).borrow() != state.key())
                                .cloned(),
                        );
                        new.push(state.payload());
                        new.shrink_to_fit();
                        let new = owned_data::<C>(new);
                        if !replace(new, true) {
                            continue;
                        }
                    }

                    return old.or_else(|| state.into_return(mode));
                }
            } else {
                // An inner node, go one level deeper.
                let inner = unsafe { node.as_ref().expect("We just checked this is not NULL") };
                let bits = (hash >> shift) & LEVEL_MASK;
                shift += LEVEL_BITS;
                parent = Some((current, node));
                current = &inner.0[bits as usize];
            }
        }
    }

    pub fn get<Q>(&self, key: &Q) -> Option<C::Payload>
    where
        Q: ?Sized + Eq + Hash,
        C::Key: Borrow<Q>,
    {
        let mut current = &self.root;
        let mut hash = self.hash(key);
        let pin = crossbeam_epoch::pin();
        loop {
            let node = current.load(Ordering::Acquire, &pin);
            let flags = nf(node);
            if node.is_null() {
                return None;
            } else if flags.contains(NodeFlags::DATA) {
                return unsafe { load_data::<C>(node) }
                    .iter()
                    .find(|l| (*l).borrow().borrow() == key)
                    .cloned();
            } else {
                let inner = unsafe { node.as_ref().expect("We just checked this is not NULL") };
                let bits = hash & LEVEL_MASK;
                hash >>= LEVEL_BITS;
                current = &inner.0[bits as usize];
            }
        }
    }

    pub fn get_or_insert_with<F>(&self, key: C::Key, create: F) -> C::Payload
    where
        F: FnOnce(C::Key) -> C::Payload,
    {
        let state = TraverseState::Future {
            key,
            constructor: create,
        };
        self.traverse(state, TraverseMode::IfMissing)
            .expect("Should have created one for me")
    }

    pub fn remove<Q>(&self, key: &Q) -> Option<C::Payload>
    where
        Q: ?Sized + Eq + Hash,
        C::Key: Borrow<Q>,
    {
        let mut current = &self.root;
        let hash = self.hash(key);
        let pin = crossbeam_epoch::pin();
        let mut shift = 0;
        let mut levels: ArrayVec<[_; MAX_LEVELS]> = ArrayVec::new();
        let deleted = loop {
            let node = current.load(Ordering::Acquire, &pin);
            let flags = nf(node);
            let replace = |with: Shared<_>| {
                let result = current.compare_and_set_weak(node, with, Ordering::Release, &pin);
                match result {
                    Ok(_) => {
                        assert!(flags.contains(NodeFlags::DATA));
                        unsafe {
                            let node = Shared::from(node.as_raw() as usize as *const Data<C>);
                            pin.defer_destroy(node);
                        }
                        true
                    }
                    Err(ref e) if !e.new.is_null() => {
                        assert!(nf(e.new).contains(NodeFlags::DATA));
                        unsafe { drop_data::<C>(e.new) };
                        false
                    }
                    Err(_) => false,
                }
            };

            if node.is_null() {
                // Nothing to delete, so just give up (without pruning).
                return None;
            } else if flags.contains(NodeFlags::CONDEMNED) {
                unsafe {
                    let (current, node) = levels.pop().expect("Condemned the root");
                    Self::prune(&pin, current, node);
                }
                // Retry by starting over from the top, for similar reasons to the one in
                // insert.
                levels.clear();
                shift = 0;
                current = &self.root;
            } else if flags.contains(NodeFlags::DATA) {
                let data = unsafe { load_data::<C>(node) };
                // Try deleting the thing.
                let mut deleted = None;
                let new = data
                    .iter()
                    .filter(|l| {
                        if (*l).borrow().borrow() == key {
                            deleted = Some((*l).clone());
                            false
                        } else {
                            true
                        }
                    })
                    .cloned()
                    .collect::<Data<C>>();

                if deleted.is_some() {
                    let new = if new.is_empty() {
                        Shared::null()
                    } else {
                        owned_data::<C>(new).into_shared(&pin)
                    };
                    if !replace(new) {
                        continue;
                    }
                }

                break deleted;
            } else {
                let inner = unsafe { node.as_ref().expect("We just checked for NULL") };
                levels.push((current, node));
                let bits = (hash >> shift) & LEVEL_MASK;
                shift += LEVEL_BITS;
                current = &inner.0[bits as usize];
            }
        };

        // Go from the top and try to clean up.
        if deleted.is_some() {
            for (parent, child) in levels.into_iter().rev() {
                let inner = unsafe { child.as_ref().expect("We just checked for NULL") };

                // This is an optimisation ‒ replacing the thing is expensive, so we want to check
                // first (which is cheaper).
                let non_null = inner
                    .0
                    .iter()
                    .filter(|ptr| !ptr.load(Ordering::Relaxed, &pin).is_null())
                    .count();
                if non_null > 1 {
                    // No reason to go into the upper levels.
                    break;
                }

                // OK, we think we could remove this node. Try doing so.
                if let PruneResult::Copy = unsafe { Self::prune(&pin, parent, child) } {
                    // Even though we tried to count how many pointers there are, someone must have
                    // added some since. So there's no way we can prone anything higher up and we
                    // give up.
                    break;
                }
                // Else:
                // Just continue with higher levels. Even if someone made the contraction for
                // us, it should be safe to do so.
            }
        }

        deleted
    }

    pub fn is_empty(&self) -> bool {
        // This relies on proper branch pruning.
        // We can use the unprotected here, because we are not actually interested in where the
        // pointer points to. Therefore we can also use the Relaxed ordering.
        unsafe {
            self.root
                .load(Ordering::Relaxed, &crossbeam_epoch::unprotected())
                .is_null()
        }
    }

    // TODO: Iteration & friends
}

impl<C: Config, S> Drop for Raw<C, S> {
    fn drop(&mut self) {
        /*
         * Notes about unsafety here:
         * * We are in a destructor and that one is &mut self. There are no concurrent accesses to
         *   this data structure any more, therefore we can safely assume we are the only ones
         *   looking at the pointers inside.
         * * Therefore, using unprotected is also fine.
         * * Similarly, the Relaxed ordering here is fine too, as the whole data structure must
         *   have been synchronized into our thread already by this time.
         * * The pointer inside this data structure is never dangling.
         */
        unsafe fn drop_recursive<C: Config>(node: &Atomic<Inner>) {
            let pin = crossbeam_epoch::unprotected();
            let extract = node.load(Ordering::Relaxed, &pin);
            let flags = nf(extract);
            if extract.is_null() {
                // Skip
            } else if flags.contains(NodeFlags::DATA) {
                drop_data::<C>(extract);
            } else {
                let owned = extract.into_owned();
                for sub in &owned.0 {
                    drop_recursive::<C>(sub);
                }
                drop(owned);
            }
        }
        unsafe { drop_recursive::<C>(&self.root) };
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::ConMap;
    use super::*;

    // A hasher to create collisions on purpose. Let's make the hash trie into a glorified array.
    // We allow tests in higher-level modules to reuse it for their tests.
    pub(crate) struct NoHasher;

    impl Hasher for NoHasher {
        fn finish(&self) -> u64 {
            0
        }

        fn write(&mut self, _: &[u8]) {}
    }

    impl BuildHasher for NoHasher {
        type Hasher = NoHasher;

        fn build_hasher(&self) -> NoHasher {
            NoHasher
        }
    }

    #[test]
    fn consts_consistent() {
        assert_eq!(LEVEL_BITS, LEVEL_MASK.count_ones() as usize);
        assert_eq!(LEVEL_BITS, (!LEVEL_MASK).trailing_zeros() as usize);
        assert_eq!(LEVEL_CELLS, 2usize.pow(LEVEL_BITS as u32));
    }
}
