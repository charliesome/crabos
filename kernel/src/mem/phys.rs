use core::fmt::{self, Debug};
use core::mem;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::critical::{self, Critical};
use crate::mem::{zero, MemoryExhausted};
use crate::mem::page::{self, PAGE_SIZE, PageFlags};
use crate::sync::Mutex;

extern "C" {
    static _phys_rc: AtomicUsize;
    static _phys_rc_end: AtomicUsize;
}

static REF_COUNT_ENABLED: AtomicBool = AtomicBool::new(false);
static PHYS_REGIONS: Mutex<Option<[PhysRegion; 8]>> = Mutex::new(None);
static NEXT_FREE_PHYS: Mutex<Option<RawPhys>> = Mutex::new(None);

const REGION_KIND_USABLE: u32 = 1;
const MAX_PHYS_PAGE: u64 = 1 << 48;

#[repr(transparent)]
#[derive(Copy, Clone, PartialOrd, Ord, PartialEq, Eq, Debug)]
pub struct RawPhys(pub u64);

pub struct Phys(u64);

impl Phys {
    /// Creates a new Phys, incrementing the reference count of the underlying
    /// physical page by one
    pub unsafe fn new(raw_phys: RawPhys) -> Phys {
        inc_ref(raw_phys);
        Phys(raw_phys.0)
    }

    /// Consumes the Phys, returning the raw address of the physical page. This
    /// method does not affect the reference count of the underlying physical
    /// page, so care must be taken to avoid leaks.
    pub fn into_raw(self) -> RawPhys {
        let phys = self.0;
        mem::forget(self);
        RawPhys(phys)
    }

    /// Constructs a Phys from a raw address returned by `into_raw`. This
    /// function is the dual of into_raw. This function does not affect the
    /// reference count of the underlying physical page, so care must be taken
    /// to only call this once per corresponding `into_raw` call.
    pub unsafe fn from_raw(raw_phys: RawPhys) -> Phys {
        Phys(raw_phys.0)
    }
}

impl Clone for Phys {
    fn clone(&self) -> Self {
        unsafe { Phys::new(RawPhys(self.0)) }
    }
}

impl Debug for Phys {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Phys(0x{:08x})", self.0)
    }
}

#[derive(Clone)]
struct PhysRegion {
    begin: RawPhys,
    size: u64,
}

#[repr(C)]
pub struct BiosMemoryRegion {
    begin: RawPhys,
    size: u64,
    kind: u32,
    acpi_ex_attrs: u32,
}

fn alloc_freelist() -> Option<Phys> {
    let mut next_free = NEXT_FREE_PHYS.lock();

    next_free.take().map(|phys| {
        unsafe {
            let crit = critical::begin();
            let phys = Phys::new(phys);

            let mapped = page::temp_map::<Option<RawPhys>>(RawPhys(phys.0), &crit);

            // pull linked next free phys out:
            *next_free = (*mapped.ptr()).take();

            // zero page before returning:
            zero(mapped.ptr() as *mut u8, PAGE_SIZE);

            phys
        }
    })
}

fn alloc_new(regions: &mut [PhysRegion]) -> Result<Phys, MemoryExhausted> {
    for region in regions {
        if region.size == 0 {
            continue;
        }

        let raw_phys = region.begin;
        region.begin.0 += PAGE_SIZE as u64;
        region.size -= PAGE_SIZE as u64;

        let phys = unsafe { Phys::new(raw_phys) };

        unsafe {
            let crit = critical::begin();

            let mapped = page::temp_map::<u64>(raw_phys, &crit);
            zero(mapped.ptr() as *mut u8, PAGE_SIZE);
        }

        return Ok(phys);
    }

    Err(MemoryExhausted)
}

pub fn alloc() -> Result<Phys, MemoryExhausted> {
    if let Some(page) = alloc_freelist() {
        return Ok(page);
    }

    alloc_new(PHYS_REGIONS.lock()
        .as_mut()
        .expect("PHYS_REGIONS is None in phys::alloc"))
}

impl Drop for Phys {
    fn drop(&mut self) {
        match dec_ref(RawPhys(self.0)) {
            PhysStatus::InUse => {}
            PhysStatus::ShouldFree => {
                let mut next_free = NEXT_FREE_PHYS.lock();

                unsafe {
                    let crit = critical::begin();
                    let link = page::temp_map::<Option<RawPhys>>(RawPhys(self.0), &crit);
                    ptr::write(link.ptr(), next_free.take());
                }

                *next_free = Some(RawPhys(self.0));
            }
        }
    }
}

pub unsafe fn init_ref_counts(_critical: &Critical) {
    // inc ref for all currently mapped pages
    page::each_phys(|raw_phys| {
        inc_ref_for_init(raw_phys);
    });

    // inc ref count for pml4
    {
        let cr3;
        asm!("movq %cr3, $0" : "=r"(cr3));
        inc_ref_for_init(RawPhys(cr3));
    }

    REF_COUNT_ENABLED.store(true, Ordering::SeqCst);
}

fn ref_count(raw: RawPhys) -> &'static AtomicUsize {
    if raw.0 > MAX_PHYS_PAGE {
        panic!("addr > MAX_PHYS_PAGE (addr = {:x?})", raw);
    }

    let base = unsafe { &_phys_rc as *const AtomicUsize };
    let end = unsafe { &_phys_rc_end as *const AtomicUsize };

    let page_number = raw.0 >> 12;
    let rc = unsafe { base.add(page_number as usize) };

    if rc > end {
        panic!("rc > end");
    }

    unsafe { &*rc }
}

fn inc_ref(raw: RawPhys) {
    if REF_COUNT_ENABLED.load(Ordering::SeqCst) {
        ref_count(raw)
            .fetch_add(1, Ordering::SeqCst);
    }
}

fn inc_ref_for_init(raw: RawPhys) {
    // always inc ref count, this function is used from init_ref_counts
    ref_count(raw)
        .fetch_add(1, Ordering::SeqCst);
}

enum PhysStatus {
    InUse,
    ShouldFree,
}

fn dec_ref(raw: RawPhys) -> PhysStatus {
    if REF_COUNT_ENABLED.load(Ordering::SeqCst) {
        let previous_ref_count = ref_count(raw)
            // TODO - we can probably do better than SeqCst here:
            .fetch_sub(1, Ordering::SeqCst);

        if previous_ref_count == 0 {
            panic!("phys::dec_ref underflowed!");
        }

        // return the current ref count as of immediately after this operation:
        if previous_ref_count == 1 {
            PhysStatus::ShouldFree
        } else {
            PhysStatus::InUse
        }
    } else {
        PhysStatus::InUse
    }
}

unsafe fn ensure_rc_page(phys: RawPhys) {
    let page = ref_count(phys);

    let ref_count_page = ((page as *const AtomicUsize) as usize & !(PAGE_SIZE - 1)) as *mut u8;

    if !page::is_mapped(ref_count_page) {
        let phys = alloc()
            .expect("phys::alloc in phys_init");

        page::map(phys, ref_count_page, PageFlags::PRESENT | PageFlags::WRITE)
            .expect("page::map in phys_init");
    }
}

#[no_mangle]
pub unsafe extern "C" fn phys_init(
    bios_memory_map: *const BiosMemoryRegion,
    region_count: u16,
    high_memory_boundary: RawPhys,
) {
    crate::println!("Initialising physical page allocator... high_memory_boundary={:?}", high_memory_boundary);

    // init temp mapping
    page::temp_reset();

    let mut phys_i = 0;

    // XXX - it's important PhysRegion is not Copy to prevent bugs from
    // unintentional copies out of the mutex-guarded static. This means we're
    // unable to use the repeat array initializer syntax here:
    const NULL_PHYS_REGION: PhysRegion = PhysRegion { begin: RawPhys(0), size: 0 };
    let mut phys_regions = [
        NULL_PHYS_REGION,
        NULL_PHYS_REGION,
        NULL_PHYS_REGION,
        NULL_PHYS_REGION,
        NULL_PHYS_REGION,
        NULL_PHYS_REGION,
        NULL_PHYS_REGION,
        NULL_PHYS_REGION,
    ];

    for i in 0..region_count {
        let region = &*bios_memory_map.add(i as usize);

        crate::println!("  - Memory region 0x{:016x}, length 0x{:016x}", region.begin.0, region.size);
        crate::println!("    type {}, acpi ex attrs 0x{:08x}", region.kind, region.acpi_ex_attrs);

        if region.kind != REGION_KIND_USABLE {
            continue;
        }

        let region_begin = region.begin;
        let region_end = RawPhys(region.begin.0 + region.size);

        if region_end < high_memory_boundary {
            continue;
        }

        let region_begin = if region_begin < high_memory_boundary {
            high_memory_boundary
        } else {
            region_begin
        };

        let region_size = region_end.0 - region_begin.0;

        crate::println!("    - registering as region #{}", phys_i);
        phys_regions[phys_i].begin = region_begin;
        phys_regions[phys_i].size = region_size;
        phys_i += 1;

        if phys_i == phys_regions.len() {
            break;
        }
    }

    let mibibytes = phys_regions.iter().map(|reg| reg.size).sum::<u64>() / 1024 / 1024;
    crate::println!("  {} MiB free", mibibytes);

    *PHYS_REGIONS.lock() = Some(phys_regions.clone());

    // map ref count pages for all allocatable phys regions
    for region in &phys_regions {
        let end = RawPhys(region.begin.0 + region.size);

        let mut phys = region.begin;

        while phys < end {
            ensure_rc_page(phys);
            phys.0 += PAGE_SIZE as u64;
        }
    }

    // map ref count pages for all low memory pages
    for i in 0..(high_memory_boundary.0 / PAGE_SIZE as u64) {
        let raw_phys = RawPhys(i * PAGE_SIZE as u64);

        ensure_rc_page(raw_phys);
    }

    crate::println!();
}
