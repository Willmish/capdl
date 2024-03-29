// Copyright 2022 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
// cantrip-os-loader: capDL bootstrap for CantripOS.
//

// Constructs a system of multiple components according to a capDL
// specification. Derived from the C version of capdl-loader-app.
//
// In addition to constructing the system components, the rootserver is
// responsible for handing off objects that need to outlive the rootserver.
// The boot sequence is:
//   1. The kernel marks UntypedMemory objects it uses to construct
//      rootserver objects as "tainted".
//   2. The kernel passes all UntypedMemory objects to the rootserver.
//   3. The rootserver runs and skips over UntypedMemory marked tainted
//      to avoid mixing rootserver resources w/ non-rootserver resources.
//   4. The rootserver hands off UntypedMemory objects to the MemoryManager.
//   5. The rootserver suspends itself.
//   6. The MemoryManager starts up and seeds it's memory pool from the
//      UntypedMemory objects passed to it. As part of this process, for
//      each UntypedMemory object marked "tainted" it issues a Revoke system
//      call that causes the kernel to reclaim all memory associated with
//      the object (including the rootserver's TCB).
// At the end of this sequence all vestiges of the rootserver are gone and
// the MemoryManager owns all available memory. The CAmkES components
// instantiated by the rootserver remain because the MemoryManager has
// references to the capDL-specified objects.

#![no_std]
#![no_main]

use cantrip_os_common::allocator;
use cantrip_os_common::sel4_sys;
use capdl;
use cfg_if::cfg_if;
use core2::io::{Cursor, Write};
use core::mem::size_of;
use core::ptr;
use log::*;
use model;

use capdl::CDL_Core;
use capdl::CDL_Model;
use capdl::CDL_ObjID;
use capdl::CDL_IRQ;

use model::CantripOsModel;
use model::ModelState;

use sel4_sys::seL4_BootInfo;
use sel4_sys::seL4_CPtr;
use sel4_sys::seL4_CapInitThreadTCB;
use sel4_sys::seL4_GetIPCBuffer;
use sel4_sys::seL4_TCB_Suspend;

// Linkage to pre-calculated data used to initialize the system.
extern "C" {
    static capdl_spec: CDL_Model; // Generated by the CapDL tools from the .cdl spec

    static __executable_start: [u8; 1]; // Start of rootserver image.
    static _end: [u8; 1];

    fn sel4runtime_bootinfo() -> *const seL4_BootInfo;
}

// Most platforms have the CAmkES components embedded in an elf segment
// exposed through these symbols. Some platforms may store the data in
// flash and retrieve it on demand using platform-specific methods.
//
// Note we depend on LTO to elide the associated elf segment when built
// without "fill_from_cpio" (e.g. "fill_from_sec"). This is subtle but
// allows us to leave the cmake BuildCapDLApplication function unchanged.
#[cfg(feature = "fill_from_cpio")]
extern "C" {
    static _capdl_archive: [u8; 1]; // CPIO archive of component images
    static _capdl_archive_end: [u8; 1];
}

// Set log level for tracing rootseerver operation.
cfg_if! {
    if #[cfg(feature = "LOG_DEBUG")] {
        const INIT_LOG_LEVEL: LevelFilter = LevelFilter::Debug;
    } else if #[cfg(feature = "LOG_TRACE")] {
        const INIT_LOG_LEVEL: LevelFilter = LevelFilter::Trace;
    } else {
        const INIT_LOG_LEVEL: LevelFilter = LevelFilter::Info;
    }
}

// This sizes data structures that are reclaimed when the rootserver
// completes, but it is still important to tune them to cap the peak
// memory used during boot. Beware these only affect the user-space
// memory of the rootserver; if the sizes need tunning then the
// kernel-created data structures likely also need tuning;
// c.f. KernelRootCNodeSizeBits & KernelMaxNumBootinfoUntypedCaps in
// kernel/config.cmake (usually set in easy-settings.cmake).
cfg_if! {
    if #[cfg(any(feature = "CONFIG_PLAT_SHODAN", feature = "CONFIG_PLAT_NEXUS"))] {
        #[cfg(not(feature = "CONFIG_DEBUG_BUILD"))]
        const CONFIG_CAPDL_LOADER_MAX_OBJECTS: usize = 1500;

        #[cfg(feature = "CONFIG_DEBUG_BUILD")]
        // NB: ~4x for debug build 'cuz of executable image sizes
        const CONFIG_CAPDL_LOADER_MAX_OBJECTS: usize = 5500;

        const CONFIG_MAX_NUM_BOOTINFO_UNTYPED_CAPS: usize = 128;
    } else {
        // NB: rpi3 has 1G of memory so no need to shrink config
        // NB: max objects is ~1/2 what the C code has because we use
        //   a copy-on-write scheme for handling shared pages which results
        //   in created ~1/2 as many capabilities.
        const CONFIG_CAPDL_LOADER_MAX_OBJECTS: usize = 10000;
        const CONFIG_MAX_NUM_BOOTINFO_UNTYPED_CAPS: usize = 230;
    }
}
const CONFIG_MAX_NUM_IRQS: usize = 128;
const CONFIG_MAX_NUM_NODES: usize = 1;

// State required to process a Model specification. We separate this from
// the implentation so callers can decide how to manage this state (and
// also for unit tests). The object tables must be large enough to hold the
// objects in the model specification + one additional for each TCB & CNode
// and one for each page Frame that is shared. On some platforms it may be
// possible dynamically allocate this storage in which case it can be sized
// according to the specification and bootinfo.
//
// XXX say something about memmory re-use after the loader completes setup.
struct CantripOsModelState {
    // Mapping from object ID (from specification) to associated object CPtr
    // created in the rootserver's CSpace.
    capdl_to_sel4_orig: [seL4_CPtr; CONFIG_CAPDL_LOADER_MAX_OBJECTS],
    // Mapping from object ID to any dup of capdl_to_sel4_orig. This is
    // used to track objects as they are moved from the rootserver's CSpace
    // to the target CSpace.
    capdl_to_sel4_dup: [seL4_CPtr; CONFIG_CAPDL_LOADER_MAX_OBJECTS],
    // Mapping from IRQ number to associated handler capability.
    capdl_to_sel4_irq: [seL4_CPtr; CONFIG_MAX_NUM_IRQS],
    // Mapping from SchedCtrl number to associated scheduler context.
    capdl_to_sched_ctrl: [seL4_CPtr; CONFIG_MAX_NUM_NODES],

    // For static object allocation, this maps from untyped derivation
    // index to cslot. For dynamic object allocation, this stores the
    // result of sort_untypeds.
    untyped_cptrs: [seL4_CPtr; CONFIG_MAX_NUM_BOOTINFO_UNTYPED_CAPS],
}
impl CantripOsModelState {
    pub const fn new() -> Self {
        CantripOsModelState {
            capdl_to_sel4_orig: [0 as seL4_CPtr; CONFIG_CAPDL_LOADER_MAX_OBJECTS],
            capdl_to_sel4_dup: [0 as seL4_CPtr; CONFIG_CAPDL_LOADER_MAX_OBJECTS],
            capdl_to_sel4_irq: [0 as seL4_CPtr; CONFIG_MAX_NUM_IRQS],
            capdl_to_sched_ctrl: [0 as seL4_CPtr; CONFIG_MAX_NUM_NODES],

            untyped_cptrs: [0 as seL4_CPtr; CONFIG_MAX_NUM_BOOTINFO_UNTYPED_CAPS],
        }
    }
}
impl ModelState for CantripOsModelState {
    fn get_max_objects(&self) -> usize {
        self.capdl_to_sel4_orig.len()
    }
    fn get_max_irqs(&self) -> usize {
        self.capdl_to_sel4_irq.len()
    }
    fn get_max_sched_ctrl(&self) -> usize {
        self.capdl_to_sched_ctrl.len()
    }
    fn get_max_untyped_caps(&self) -> usize {
        self.untyped_cptrs.len()
    }

    fn get_orig_cap(&self, obj_id: CDL_ObjID) -> seL4_CPtr {
        self.capdl_to_sel4_orig[obj_id]
    }
    fn set_orig_cap(&mut self, obj_id: CDL_ObjID, slot: seL4_CPtr) {
        self.capdl_to_sel4_orig[obj_id] = slot;
    }

    fn get_dup_cap(&self, obj_id: CDL_ObjID) -> seL4_CPtr {
        self.capdl_to_sel4_dup[obj_id]
    }
    fn set_dup_cap(&mut self, obj_id: CDL_ObjID, slot: seL4_CPtr) {
        self.capdl_to_sel4_dup[obj_id] = slot;
    }

    fn get_irq_cap(&self, irq: CDL_IRQ) -> seL4_CPtr {
        self.capdl_to_sel4_irq[irq]
    }
    fn set_irq_cap(&mut self, irq: CDL_IRQ, slot: seL4_CPtr) {
        self.capdl_to_sel4_irq[irq] = slot;
    }

    fn get_sched_ctrl_cap(&self, id: CDL_Core) -> seL4_CPtr {
        self.capdl_to_sched_ctrl[id]
    }
    fn set_sched_ctrl_cap(&mut self, id: CDL_Core, slot: seL4_CPtr) {
        self.capdl_to_sched_ctrl[id] = slot;
    }

    fn get_untyped_cptr(&self, ix: usize) -> seL4_CPtr {
        self.untyped_cptrs[ix]
    }
    fn set_untyped_cptr(&mut self, ix: usize, slot: seL4_CPtr) {
        self.untyped_cptrs[ix] = slot
    }
}

// Console output is sent through the log crate. We use seL4_DebugPutChar
// to write to the console which only works if DEBUG_PRINTING is enabled
// in the kernel. Note this differs from capdl-loader-app which uses
// sel4platformsupport to write to the console/uart.
struct CapdlLogger;
impl log::Log for CapdlLogger  {
    fn enabled(&self, _metadata: &Metadata) -> bool { true }
    fn flush(&self) {}
    fn log(&self, record: &Record) {
        let mut buf = [0u8; 1024];
        let mut cur =  Cursor::new(&mut buf[..]);
        write!(&mut cur, "{}:{}", record.target(), record.args()).unwrap_or_else(|_| {
            cur.set_position((1024 - 3) as u64);
            cur.write(b"...").expect("write");
        });
        let pos = cur.position() as usize;

        #[cfg(feature = "CONFIG_PRINTING")]
        unsafe {
            for c in &buf[..pos] {
                let _ = sel4_sys::seL4_DebugPutChar(*c);
            }
            let _ = sel4_sys::seL4_DebugPutChar(b'\n');
        }
    }
}

#[no_mangle]
pub fn main() {
    // Setup logger.
    static CAPDL_LOGGER: CapdlLogger = CapdlLogger;
    log::set_logger(&CAPDL_LOGGER).unwrap();
    log::set_max_level(INIT_LOG_LEVEL);

    // Setup memory allocation from a fixed heap. For the configurations
    // tested no heap was used. CantripOsModel may use the heap if the model
    // has many VSpace roots.
    static mut HEAP_MEMORY: [u8; 4096] = [0; 4096];
    unsafe {
        allocator::ALLOCATOR.init(HEAP_MEMORY.as_mut_ptr(), HEAP_MEMORY.len());
        trace!(
            "setup heap: start_addr {:p} size {}",
            HEAP_MEMORY.as_ptr(),
            HEAP_MEMORY.len()
        );
    }

    let capdl_spec_ref = unsafe { &capdl_spec };
    let bootinfo_ref = unsafe { &*sel4runtime_bootinfo() };

    // Verify the IPC buffer is setup correctly for system calls. In
    // particular we need Rust's tls-model to match what the kernel uses.
    assert_eq!(unsafe { seL4_GetIPCBuffer() }, bootinfo_ref.ipcBuffer);

    // Check the compile-time-configuration against the bootinfo. We need
    // space to track all seL4 objects that may be created. Note bootinfo
    // only identifies the kernel setup. minimum number of
    // entries; more are created when we dup objects (CNode's and TCB's)
    // and if we clone shared page Frame obects. If we run out of space
    // an assert will trip when calling one of the ModelObject traits.
    //
    // NB: on platforms that use libsel4platsupport additional storage
    //     will be (silently) used;
    info!(
        "Bootinfo: {:?} empty slots {} nodes {:?} untyped {} cnode slots",
        (bootinfo_ref.empty.start, bootinfo_ref.empty.end),
        bootinfo_ref.numNodes,
        (bootinfo_ref.untyped.start, bootinfo_ref.untyped.end),
        1 << bootinfo_ref.initThreadCNodeSizeBits
    );
    info!(
        "Model: {} objects {} irqs {} untypeds {} asids",
        capdl_spec_ref.num,
        capdl_spec_ref.num_irqs,
        capdl_spec_ref.num_untyped,
        capdl_spec_ref.num_asid_slots
    );
    assert!(
        bootinfo_ref.empty.end - bootinfo_ref.empty.start >= CONFIG_CAPDL_LOADER_MAX_OBJECTS,
        "Not enough object storage: bootinfo has {} but CONFIG_CAPDL_LOADER_MAX_OBJECTS={}",
        bootinfo_ref.empty.end - bootinfo_ref.empty.start,
        CONFIG_CAPDL_LOADER_MAX_OBJECTS
    );

    fn calc_bytes(begin: *const u8, end: *const u8) -> usize {
        (end as usize) - (begin as usize)
    }

    #[cfg(feature = "fill_from_cpio")]
    let capdl_archive_ref = unsafe {
        core::slice::from_raw_parts(
            ptr::addr_of!(_capdl_archive[0]),
            calc_bytes(
                ptr::addr_of!(_capdl_archive[0]),
                ptr::addr_of!(_capdl_archive_end[0]),
            ),
        )
    };
    #[cfg(not(feature = "fill_from_cpio"))]
    let capdl_archive_ref = &[0u8; 0];

    let executable_ref = unsafe {
        core::slice::from_raw_parts(
            ptr::addr_of!(__executable_start[0]),
            calc_bytes(ptr::addr_of!(__executable_start[0]), ptr::addr_of!(_end[0])),
        )
    };

    fn to_megabytes(bytes: usize) -> f32 {
        bytes as f32 / (1024. * 1024.)
    }
    let capdl_space = capdl_spec_ref.calc_space();
    info!("capDL spec: {:.2} Mbytes", to_megabytes(capdl_space));
    info!(
        "CAmkES components: {:.2} Mbytes",
        to_megabytes(capdl_archive_ref.len())
    );
    info!(
        "Rootserver executable: {:.2} Mbytes",
        to_megabytes(executable_ref.len() - (capdl_space + capdl_archive_ref.len()))
    );

    // The model goes on the stack which usually has a fixed & limited size.
    // We don't know what's been configured but the default is 16KB; require
    // no more than 1/2 the stack space is used to hold it. Note
    // CantripOsModelState holds all the large data structures; CantripOsModel's
    // size mostly depends on how space is given to vspace_roots.
    assert!(size_of::<CantripOsModel>() < (16 * 1024 / 2));

    // NB: STATE does not fit on the stack or heap.
    static mut STATE: CantripOsModelState = CantripOsModelState::new();
    let mut model = CantripOsModel::new(
        unsafe { &mut STATE },
        capdl_spec_ref,
        bootinfo_ref,
        capdl_archive_ref,
        executable_ref,
    );
    model.init_system().expect("init_system");

    // Log info about key data structure usage.
    info!(
        "Rootserver cnode: {} used of {}",
        model.get_free_slot(),
        unsafe { STATE.get_max_objects() }
    );
    info!(
        "Rootserver untypeds: {} used of {}",
        unsafe {
            STATE
                .untyped_cptrs
                .iter()
                .filter_map(|&v| if v != 0 { Some(v) } else { None })
                .max()
        }
        .unwrap_or(0),
        unsafe { STATE.get_max_untyped_caps() },
    );

    // Hand-off the rootserver's resources (typically to the MemoryManager).
    // NB: this includes the tainted UntypedMemory objects that when revoked
    //   will cause the rootserver's memory to be returned to the free pool.
    model.handoff_capabilities().expect("handoff_capabilities");

    model.start_threads().expect("start_threads");

    let _ = unsafe { seL4_TCB_Suspend(seL4_CapInitThreadTCB) };
}
