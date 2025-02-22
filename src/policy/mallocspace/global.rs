use super::metadata::*;
use crate::plan::ObjectQueue;
use crate::plan::VectorObjectQueue;
use crate::policy::space::CommonSpace;
use crate::policy::space::SFT;
use crate::util::constants::BYTES_IN_PAGE;
use crate::util::heap::PageResource;
use crate::util::malloc::malloc_ms_util::*;
use crate::util::metadata::side_metadata::{
    bzero_metadata, SideMetadataContext, SideMetadataSanity, SideMetadataSpec,
};
use crate::util::metadata::MetadataSpec;
use crate::util::opaque_pointer::*;
use crate::util::Address;
use crate::util::ObjectReference;
use crate::util::{conversions, metadata};
use crate::vm::VMBinding;
use crate::vm::{ActivePlan, Collection, ObjectModel};
use crate::{policy::space::Space, util::heap::layout::vm_layout_constants::BYTES_IN_CHUNK};
use std::marker::PhantomData;
#[cfg(debug_assertions)]
use std::sync::atomic::AtomicU32;
use std::sync::atomic::{AtomicUsize, Ordering};
// only used for debugging
use crate::policy::space::*;
#[cfg(debug_assertions)]
use std::collections::HashMap;
#[cfg(debug_assertions)]
use std::sync::Mutex;

// If true, we will use a hashmap to store all the allocated memory from malloc, and use it
// to make sure our allocation is correct.
#[cfg(debug_assertions)]
const ASSERT_ALLOCATION: bool = false;

pub struct MallocSpace<VM: VMBinding> {
    phantom: PhantomData<VM>,
    active_bytes: AtomicUsize,
    pub chunk_addr_min: AtomicUsize, // XXX: have to use AtomicUsize to represent an Address
    pub chunk_addr_max: AtomicUsize,
    metadata: SideMetadataContext,
    // Mapping between allocated address and its size - this is used to check correctness.
    // Size will be set to zero when the memory is freed.
    #[cfg(debug_assertions)]
    active_mem: Mutex<HashMap<Address, usize>>,
    // The following fields are used for checking correctness of the parallel sweep implementation
    // as we need to check how many live bytes exist against `active_bytes` when the last sweep
    // work packet is executed
    #[cfg(debug_assertions)]
    pub total_work_packets: AtomicU32,
    #[cfg(debug_assertions)]
    pub completed_work_packets: AtomicU32,
    #[cfg(debug_assertions)]
    pub work_live_bytes: AtomicUsize,
}

impl<VM: VMBinding> SFT for MallocSpace<VM> {
    fn name(&self) -> &str {
        self.get_name()
    }

    fn is_live(&self, object: ObjectReference) -> bool {
        is_marked::<VM>(object, Some(Ordering::SeqCst))
    }

    fn is_movable(&self) -> bool {
        false
    }

    #[cfg(feature = "sanity")]
    fn is_sane(&self) -> bool {
        true
    }

    // For malloc space, we need to further check the alloc bit.
    fn is_in_space(&self, object: ObjectReference) -> bool {
        is_alloced_by_malloc(object)
    }

    /// For malloc space, we just use the side metadata.
    #[cfg(feature = "is_mmtk_object")]
    #[inline(always)]
    fn is_mmtk_object(&self, addr: Address) -> bool {
        debug_assert!(!addr.is_zero());
        // `addr` cannot be mapped by us. It should be mapped by the malloc library.
        debug_assert!(!addr.is_mapped());
        has_object_alloced_by_malloc(addr)
    }

    fn initialize_object_metadata(&self, object: ObjectReference, _alloc: bool) {
        trace!("initialize_object_metadata for object {}", object);
        let page_addr = conversions::page_align_down(object.to_address());
        set_page_mark(page_addr);
        set_alloc_bit(object);
    }

    #[inline(always)]
    fn sft_trace_object(
        &self,
        queue: &mut VectorObjectQueue,
        object: ObjectReference,
        _worker: GCWorkerMutRef,
    ) -> ObjectReference {
        self.trace_object(queue, object)
    }
}

impl<VM: VMBinding> Space<VM> for MallocSpace<VM> {
    fn as_space(&self) -> &dyn Space<VM> {
        self
    }

    fn as_sft(&self) -> &(dyn SFT + Sync + 'static) {
        self
    }

    fn get_page_resource(&self) -> &dyn PageResource<VM> {
        unreachable!()
    }

    fn common(&self) -> &CommonSpace<VM> {
        unreachable!()
    }

    fn initialize_sft(&self) {
        // Do nothing - we will set sft when we get new results from malloc
    }

    fn release_multiple_pages(&mut self, _start: Address) {
        unreachable!()
    }

    // We have assertions in a debug build. We allow this pattern for the release build.
    #[allow(clippy::let_and_return)]
    fn in_space(&self, object: ObjectReference) -> bool {
        let ret = is_alloced_by_malloc(object);

        #[cfg(debug_assertions)]
        if ASSERT_ALLOCATION {
            let addr = VM::VMObjectModel::object_start_ref(object);
            let active_mem = self.active_mem.lock().unwrap();
            if ret {
                // The alloc bit tells that the object is in space.
                debug_assert!(
                    *active_mem.get(&addr).unwrap() != 0,
                    "active mem check failed for {} (object {}) - was freed",
                    addr,
                    object
                );
            } else {
                // The alloc bit tells that the object is not in space. It could never be allocated, or have been freed.
                debug_assert!(
                    (!active_mem.contains_key(&addr))
                        || (active_mem.contains_key(&addr) && *active_mem.get(&addr).unwrap() == 0),
                    "mem check failed for {} (object {}): allocated = {}, size = {:?}",
                    addr,
                    object,
                    active_mem.contains_key(&addr),
                    if active_mem.contains_key(&addr) {
                        active_mem.get(&addr)
                    } else {
                        None
                    }
                );
            }
        }
        ret
    }

    fn address_in_space(&self, _start: Address) -> bool {
        unreachable!("We do not know if an address is in malloc space. Use in_space() to check if an object is in malloc space.")
    }

    fn get_name(&self) -> &'static str {
        "MallocSpace"
    }

    fn reserved_pages(&self) -> usize {
        // TODO: figure out a better way to get the total number of active pages from the metadata
        let data_pages = conversions::bytes_to_pages_up(self.active_bytes.load(Ordering::SeqCst));
        let meta_pages = self.metadata.calculate_reserved_pages(data_pages);
        data_pages + meta_pages
    }

    fn verify_side_metadata_sanity(&self, side_metadata_sanity_checker: &mut SideMetadataSanity) {
        side_metadata_sanity_checker
            .verify_metadata_context(std::any::type_name::<Self>(), &self.metadata)
    }
}

use crate::scheduler::GCWorker;
use crate::util::copy::CopySemantics;

impl<VM: VMBinding> crate::policy::gc_work::PolicyTraceObject<VM> for MallocSpace<VM> {
    #[inline(always)]
    fn trace_object<Q: ObjectQueue, const KIND: crate::policy::gc_work::TraceKind>(
        &self,
        queue: &mut Q,
        object: ObjectReference,
        _copy: Option<CopySemantics>,
        _worker: &mut GCWorker<VM>,
    ) -> ObjectReference {
        self.trace_object(queue, object)
    }

    #[inline(always)]
    fn may_move_objects<const KIND: crate::policy::gc_work::TraceKind>() -> bool {
        false
    }
}

impl<VM: VMBinding> MallocSpace<VM> {
    pub fn new(global_side_metadata_specs: Vec<SideMetadataSpec>) -> Self {
        MallocSpace {
            phantom: PhantomData,
            active_bytes: AtomicUsize::new(0),
            chunk_addr_min: AtomicUsize::new(usize::max_value()), // XXX: have to use AtomicUsize to represent an Address
            chunk_addr_max: AtomicUsize::new(0),
            metadata: SideMetadataContext {
                global: global_side_metadata_specs,
                local: metadata::extract_side_metadata(&[
                    MetadataSpec::OnSide(ACTIVE_PAGE_METADATA_SPEC),
                    MetadataSpec::OnSide(OFFSET_MALLOC_METADATA_SPEC),
                    *VM::VMObjectModel::LOCAL_MARK_BIT_SPEC,
                ]),
            },
            #[cfg(debug_assertions)]
            active_mem: Mutex::new(HashMap::new()),
            #[cfg(debug_assertions)]
            total_work_packets: AtomicU32::new(0),
            #[cfg(debug_assertions)]
            completed_work_packets: AtomicU32::new(0),
            #[cfg(debug_assertions)]
            work_live_bytes: AtomicUsize::new(0),
        }
    }

    pub fn alloc(&self, tls: VMThread, size: usize, align: usize, offset: isize) -> Address {
        // TODO: Should refactor this and Space.acquire()
        if VM::VMActivePlan::global().poll(false, Some(self)) {
            assert!(VM::VMActivePlan::is_mutator(tls), "Polling in GC worker");
            VM::VMCollection::block_for_gc(VMMutatorThread(tls));
            return unsafe { Address::zero() };
        }

        let (address, is_offset_malloc) = alloc::<VM>(size, align, offset);
        if !address.is_zero() {
            let actual_size = get_malloc_usable_size(address, is_offset_malloc);

            // If the side metadata for the address has not yet been mapped, we will map all the side metadata for the range [address, address + actual_size).
            if !is_meta_space_mapped(address, actual_size) {
                // Map the metadata space for the associated chunk
                self.map_metadata_and_update_bound(address, actual_size);
                // Update SFT
                crate::mmtk::SFT_MAP.update(self, address, actual_size);
            }
            self.active_bytes.fetch_add(actual_size, Ordering::SeqCst);

            if is_offset_malloc {
                set_offset_malloc_bit(address);
            }

            #[cfg(debug_assertions)]
            if ASSERT_ALLOCATION {
                debug_assert!(actual_size != 0);
                self.active_mem.lock().unwrap().insert(address, actual_size);
            }
        }

        address
    }

    pub fn free(&self, addr: Address) {
        let offset_malloc_bit = is_offset_malloc(addr);
        let bytes = get_malloc_usable_size(addr, offset_malloc_bit);
        self.free_internal(addr, bytes, offset_malloc_bit);
    }

    // XXX optimize: We pass the bytes in to free as otherwise there were multiple
    // indirect call instructions in the generated assembly
    fn free_internal(&self, addr: Address, bytes: usize, offset_malloc_bit: bool) {
        if offset_malloc_bit {
            trace!("Free memory {:x}", addr);
            offset_free(addr);
            unsafe { unset_offset_malloc_bit_unsafe(addr) };
        } else {
            let ptr = addr.to_mut_ptr();
            trace!("Free memory {:?}", ptr);
            unsafe {
                free(ptr);
            }
        }

        self.active_bytes.fetch_sub(bytes, Ordering::SeqCst);

        #[cfg(debug_assertions)]
        if ASSERT_ALLOCATION {
            self.active_mem.lock().unwrap().insert(addr, 0).unwrap();
        }
    }

    #[inline]
    pub fn trace_object<Q: ObjectQueue>(
        &self,
        queue: &mut Q,
        object: ObjectReference,
    ) -> ObjectReference {
        if object.is_null() {
            return object;
        }

        let address = object.to_address();
        assert!(
            self.in_space(object),
            "Cannot mark an object {} that was not alloced by malloc.",
            address,
        );

        if !is_marked::<VM>(object, None) {
            let chunk_start = conversions::chunk_align_down(address);
            set_mark_bit::<VM>(object, Some(Ordering::SeqCst));
            set_chunk_mark(chunk_start);
            queue.enqueue(object);
        }

        object
    }

    fn map_metadata_and_update_bound(&self, addr: Address, size: usize) {
        // Map the metadata space for the range [addr, addr + size)
        map_meta_space(&self.metadata, addr, size);

        // Update the bounds of the max and min chunk addresses seen -- this is used later in the sweep
        // Lockless compare-and-swap loops perform better than a locking variant

        // Update chunk_addr_min, basing on the start of the allocation: addr.
        {
            let min_chunk_start = conversions::chunk_align_down(addr);
            let min_chunk_usize = min_chunk_start.as_usize();
            let mut min = self.chunk_addr_min.load(Ordering::Relaxed);
            while min_chunk_usize < min {
                match self.chunk_addr_min.compare_exchange_weak(
                    min,
                    min_chunk_usize,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(x) => min = x,
                }
            }
        }

        // Update chunk_addr_max, basing on the end of the allocation: addr + size.
        {
            let max_chunk_start = conversions::chunk_align_down(addr + size);
            let max_chunk_usize = max_chunk_start.as_usize();
            let mut max = self.chunk_addr_max.load(Ordering::Relaxed);
            while max_chunk_usize > max {
                match self.chunk_addr_max.compare_exchange_weak(
                    max,
                    max_chunk_usize,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(x) => max = x,
                }
            }
        }
    }

    pub fn sweep_chunk(&self, chunk_start: Address) {
        // Call the relevant sweep function depending on the location of the mark bits
        match *VM::VMObjectModel::LOCAL_MARK_BIT_SPEC {
            MetadataSpec::OnSide(local_mark_bit_side_spec) => {
                self.sweep_chunk_mark_on_side(chunk_start, local_mark_bit_side_spec);
            }
            _ => {
                self.sweep_chunk_mark_in_header(chunk_start);
            }
        }
    }

    /// Given an object in MallocSpace, return its malloc address, whether it is an offset malloc, and malloc size
    #[inline(always)]
    fn get_malloc_addr_size(object: ObjectReference) -> (Address, bool, usize) {
        let obj_start = VM::VMObjectModel::object_start_ref(object);
        let offset_malloc_bit = is_offset_malloc(obj_start);
        let bytes = get_malloc_usable_size(obj_start, offset_malloc_bit);
        (obj_start, offset_malloc_bit, bytes)
    }

    /// Clean up for an empty chunk
    fn clean_up_empty_chunk(&self, chunk_start: Address) {
        // Since the chunk mark metadata is a byte, we don't need synchronization
        unsafe { unset_chunk_mark_unsafe(chunk_start) };
        // Clear the SFT entry
        crate::mmtk::SFT_MAP.clear(chunk_start);
    }

    /// Sweep an object if it is dead, and unset page marks for empty pages before this object.
    /// Return true if the object is swept.
    fn sweep_object(&self, object: ObjectReference, empty_page_start: &mut Address) -> bool {
        let (obj_start, offset_malloc, bytes) = Self::get_malloc_addr_size(object);

        if !is_marked::<VM>(object, None) {
            // Dead object
            trace!("Object {} has been allocated but not marked", object);

            // Free object
            self.free_internal(obj_start, bytes, offset_malloc);
            trace!("free object {}", object);
            unsafe { unset_alloc_bit_unsafe(object) };

            true
        } else {
            // Live object that we have marked

            // Unset marks for free pages and update last_object_end
            if !empty_page_start.is_zero() {
                // unset marks for pages since last object
                let current_page = object.to_address().align_down(BYTES_IN_PAGE);

                let mut page = *empty_page_start;
                while page < current_page {
                    unsafe { unset_page_mark_unsafe(page) };
                    page += BYTES_IN_PAGE;
                }
            }

            // Update last_object_end
            *empty_page_start = (obj_start + bytes).align_up(BYTES_IN_PAGE);

            false
        }
    }

    /// Used when each chunk is done. Only called in debug build.
    #[cfg(debug_assertions)]
    fn debug_sweep_chunk_done(&self, live_bytes_in_the_chunk: usize) {
        debug!(
            "Used bytes after releasing: {}",
            self.active_bytes.load(Ordering::SeqCst)
        );

        let completed_packets = self.completed_work_packets.fetch_add(1, Ordering::SeqCst) + 1;
        self.work_live_bytes
            .fetch_add(live_bytes_in_the_chunk, Ordering::SeqCst);

        if completed_packets == self.total_work_packets.load(Ordering::Relaxed) {
            trace!(
                "work_live_bytes = {}, live_bytes = {}, active_bytes = {}",
                self.work_live_bytes.load(Ordering::Relaxed),
                live_bytes_in_the_chunk,
                self.active_bytes.load(Ordering::Relaxed)
            );
            debug_assert_eq!(
                self.work_live_bytes.load(Ordering::Relaxed),
                self.active_bytes.load(Ordering::Relaxed)
            );
        }
    }

    /// This function is called when the mark bits sit on the side metadata.
    /// This has been optimized with the use of bulk loading and bulk zeroing of
    /// metadata.
    ///
    /// This function uses non-atomic accesses to side metadata (although these
    /// non-atomic accesses should not have race conditions associated with them)
    /// as well as calls libc functions (`malloc_usable_size()`, `free()`)
    fn sweep_chunk_mark_on_side(&self, chunk_start: Address, mark_bit_spec: SideMetadataSpec) {
        #[cfg(debug_assertions)]
        let mut live_bytes = 0;

        debug!("Check active chunk {:?}", chunk_start);
        let mut address = chunk_start;
        let chunk_end = chunk_start + BYTES_IN_CHUNK;

        debug_assert!(
            crate::util::alloc_bit::ALLOC_SIDE_METADATA_SPEC.log_bytes_in_region
                == mark_bit_spec.log_bytes_in_region,
            "Alloc-bit and mark-bit metadata have different minimum object sizes!"
        );

        // For bulk xor'ing 128-bit vectors on architectures with vector instructions
        // Each bit represents an object of LOG_MIN_OBJ_SIZE size
        let bulk_load_size: usize =
            128 * (1 << crate::util::alloc_bit::ALLOC_SIDE_METADATA_SPEC.log_bytes_in_region);

        // The start of a possibly empty page. This will be updated during the sweeping, and always points to the next page of last live objects.
        let mut empty_page_start = Address::ZERO;

        // Scan the chunk by every 'bulk_load_size' region.
        while address < chunk_end {
            let alloc_128: u128 =
                unsafe { load128(&crate::util::alloc_bit::ALLOC_SIDE_METADATA_SPEC, address) };
            let mark_128: u128 = unsafe { load128(&mark_bit_spec, address) };

            // Check if there are dead objects in the bulk loaded region
            if alloc_128 ^ mark_128 != 0 {
                let end = address + bulk_load_size;

                // We will do non atomic load on the alloc bit, as this is the only thread that access the alloc bit for a chunk.
                // Linear scan through the bulk load region.
                let bulk_load_scan = crate::util::linear_scan::ObjectIterator::<
                    VM,
                    MallocObjectSize<VM>,
                    false,
                >::new(address, end);
                for object in bulk_load_scan {
                    self.sweep_object(object, &mut empty_page_start);
                }
            } else {
                // TODO we aren't actually accounting for the case where an object is alive and spans
                // a page boundary as we don't know what the object sizes are/what is alive in the bulk region
                if alloc_128 != 0 {
                    empty_page_start = address + bulk_load_size;
                }
            }

            // We have processed this bulk load memory. Step to the next.
            address += bulk_load_size;
            debug_assert!(address.is_aligned_to(bulk_load_size));
        }

        // Linear scan through the chunk, and add up all the live object sizes.
        // We have to do this as a separate pass, as in the above pass, we did not go through all the live objects
        #[cfg(debug_assertions)]
        {
            let chunk_linear_scan = crate::util::linear_scan::ObjectIterator::<
                VM,
                MallocObjectSize<VM>,
                false,
            >::new(chunk_start, chunk_end);
            for object in chunk_linear_scan {
                let (obj_start, _, bytes) = Self::get_malloc_addr_size(object);

                if ASSERT_ALLOCATION {
                    debug_assert!(
                        self.active_mem.lock().unwrap().contains_key(&obj_start),
                        "Address {} with alloc bit is not in active_mem",
                        obj_start
                    );
                    debug_assert_eq!(
                        self.active_mem.lock().unwrap().get(&obj_start),
                        Some(&bytes),
                        "Address {} size in active_mem does not match the size from malloc_usable_size",
                        obj_start
                    );
                }

                debug_assert!(
                    is_marked::<VM>(object, None),
                    "Dead object = {} found after sweep",
                    object
                );

                live_bytes += bytes;
            }
        }

        // Clear all the mark bits
        bzero_metadata(&mark_bit_spec, chunk_start, BYTES_IN_CHUNK);

        // If we never updated empty_page_start, the entire chunk is empty.
        if empty_page_start.is_zero() {
            self.clean_up_empty_chunk(chunk_start);
        }

        #[cfg(debug_assertions)]
        self.debug_sweep_chunk_done(live_bytes);
    }

    /// This sweep function is called when the mark bit sits in the object header
    ///
    /// This function uses non-atomic accesses to side metadata (although these
    /// non-atomic accesses should not have race conditions associated with them)
    /// as well as calls libc functions (`malloc_usable_size()`, `free()`)
    fn sweep_chunk_mark_in_header(&self, chunk_start: Address) {
        #[cfg(debug_assertions)]
        let mut live_bytes = 0;

        debug!("Check active chunk {:?}", chunk_start);

        // The start of a possibly empty page. This will be updated during the sweeping, and always points to the next page of last live objects.
        let mut empty_page_start = Address::ZERO;

        let chunk_linear_scan = crate::util::linear_scan::ObjectIterator::<
            VM,
            MallocObjectSize<VM>,
            false,
        >::new(chunk_start, chunk_start + BYTES_IN_CHUNK);
        for object in chunk_linear_scan {
            #[cfg(debug_assertions)]
            if ASSERT_ALLOCATION {
                let (obj_start, _, bytes) = Self::get_malloc_addr_size(object);
                debug_assert!(
                    self.active_mem.lock().unwrap().contains_key(&obj_start),
                    "Address {} with alloc bit is not in active_mem",
                    obj_start
                );
                debug_assert_eq!(
                    self.active_mem.lock().unwrap().get(&obj_start),
                    Some(&bytes),
                    "Address {} size in active_mem does not match the size from malloc_usable_size",
                    obj_start
                );
            }

            let live = !self.sweep_object(object, &mut empty_page_start);
            if live {
                // Live object. Unset mark bit
                unset_mark_bit::<VM>(object, None);

                #[cfg(debug_assertions)]
                {
                    // Accumulate live bytes
                    let (_, _, bytes) = Self::get_malloc_addr_size(object);
                    live_bytes += bytes;
                }
            }
        }

        // If we never updated empty_page_start, the entire chunk is empty.
        if empty_page_start.is_zero() {
            self.clean_up_empty_chunk(chunk_start);
        }

        #[cfg(debug_assertions)]
        self.debug_sweep_chunk_done(live_bytes);
    }
}

struct MallocObjectSize<VM>(PhantomData<VM>);
impl<VM: VMBinding> crate::util::linear_scan::LinearScanObjectSize for MallocObjectSize<VM> {
    #[inline(always)]
    fn size(object: ObjectReference) -> usize {
        let (_, _, bytes) = MallocSpace::<VM>::get_malloc_addr_size(object);
        bytes
    }
}
