use core::ptr::addr_of_mut;

use core_simd::simd::*;

use crate::collections::CInlineVec;
use crate::graph::flags::{SectionFlag, SectionFlagSet};
use crate::graph::local::LocalCoordContext;
use crate::graph::SortedRegionRenderLists;
use crate::math::*;
use crate::mem::InitDefaultInPlace;

pub const SECTIONS_IN_REGION: usize = 8 * 4 * 8;
pub const REGION_COORD_SHIFT: u8x3 = Simd::from_array([3, 2, 3]);
pub const REGION_MASK: u8x3 = Simd::from_array([0b11111000, 0b11111100, 0b11111000]);

// the graph should be region-aligned, so this should always hold true
pub const REGIONS_IN_GRAPH: usize = (256 / 8) * (256 / 4) * (256 / 8);

#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct LocalRegionIndex(u16);

impl LocalRegionIndex {
    const X_MASK_SINGLE: u16 = 0b11111000;
    const Y_MASK_SINGLE: u16 = 0b11111100;
    const Z_MASK_SINGLE: u16 = 0b11111000;

    const X_MASK_SHIFT_LEFT: u16 = 8;
    const Y_MASK_SHIFT_LEFT: u16 = 3;
    const Z_MASK_SHIFT_RIGHT: u16 = 3;

    pub fn from_local_section(local_section_coord: u8x3) -> Self {
        Self(
            ((local_section_coord.cast::<u16>()
                & u16x3::from_array([
                    Self::X_MASK_SINGLE,
                    Self::Y_MASK_SINGLE,
                    Self::Z_MASK_SINGLE,
                ]) << u16x3::from_array([Self::X_MASK_SHIFT_LEFT, Self::Y_MASK_SHIFT_LEFT, 0]))
                >> u16x3::from_array([0, 0, Self::Z_MASK_SHIFT_RIGHT]))
            .reduce_or(),
        )
    }
}

#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct RegionSectionIndex(u8);

impl RegionSectionIndex {
    const X_MASK_SINGLE: u8 = 0b00000111;
    const Y_MASK_SINGLE: u8 = 0b00000011;
    const Z_MASK_SINGLE: u8 = 0b00000111;

    const X_MASK_SHIFT: u8 = 5;
    const Y_MASK_SHIFT: u8 = 0;
    const Z_MASK_SHIFT: u8 = 2;

    pub fn from_local_section(local_section_coord: u8x3) -> Self {
        Self(
            (local_section_coord
                & u8x3::from_array([
                    Self::X_MASK_SINGLE,
                    Self::Y_MASK_SINGLE,
                    Self::Z_MASK_SINGLE,
                ]) << u8x3::from_array([
                    Self::X_MASK_SHIFT,
                    Self::Y_MASK_SHIFT,
                    Self::Z_MASK_SHIFT,
                ]))
            .reduce_or(),
        )
    }
}

#[derive(Copy, Clone)]
#[repr(C)]
pub struct RegionRenderList {
    region_coords: i32x3,
    sections_with_geometry: CInlineVec<RegionSectionIndex, SECTIONS_IN_REGION>,
    sections_with_sprites: CInlineVec<RegionSectionIndex, SECTIONS_IN_REGION>,
    sections_with_block_entities: CInlineVec<RegionSectionIndex, SECTIONS_IN_REGION>,
}

impl RegionRenderList {
    pub const UNDEFINED_REGION_COORDS: i32x3 = Simd::from_array([i32::MIN; 3]);

    pub fn add_section(&mut self, section_flags: SectionFlagSet, local_section_coord: u8x3) {
        let region_section_index = RegionSectionIndex::from_local_section(local_section_coord);
        // only add to each section list if the flag is satisfied
        self.sections_with_geometry.push_conditionally(
            region_section_index,
            section_flags.contains(SectionFlag::HasBlockGeometry),
        );
        self.sections_with_sprites.push_conditionally(
            region_section_index,
            section_flags.contains(SectionFlag::HasAnimatedSprites),
        );
        self.sections_with_block_entities.push_conditionally(
            region_section_index,
            section_flags.contains(SectionFlag::HasBlockEntities),
        );
    }

    pub fn is_initialized(&self) -> bool {
        self.region_coords != Self::UNDEFINED_REGION_COORDS
    }

    pub fn initialize(&mut self, region_coords: i32x3) {
        self.region_coords = region_coords;
    }

    pub fn is_empty(&self) -> bool {
        // this is safe because we know that the sum of the element counts can never
        // overflow, due to the maximum sizes of the vectors
        self.sections_with_geometry.element_count()
            + self.sections_with_sprites.element_count()
            + self.sections_with_block_entities.element_count()
            == 0
    }

    pub fn clear(&mut self) {
        self.region_coords = Self::UNDEFINED_REGION_COORDS;
        self.sections_with_geometry.clear();
        self.sections_with_sprites.clear();
        self.sections_with_block_entities.clear();
    }
}

impl Default for RegionRenderList {
    fn default() -> Self {
        Self {
            region_coords: Self::UNDEFINED_REGION_COORDS,
            sections_with_geometry: Default::default(),
            sections_with_sprites: Default::default(),
            sections_with_block_entities: Default::default(),
        }
    }
}

impl InitDefaultInPlace for *mut RegionRenderList {
    fn init_default_in_place(self) {
        unsafe {
            addr_of_mut!((*self).region_coords).write(RegionRenderList::UNDEFINED_REGION_COORDS);
            addr_of_mut!((*self).sections_with_geometry).init_default_in_place();
            addr_of_mut!((*self).sections_with_sprites).init_default_in_place();
            addr_of_mut!((*self).sections_with_block_entities).init_default_in_place();
        }
    }
}

#[repr(C)]
pub struct StagingRegionRenderLists {
    ordered_region_indices: CInlineVec<LocalRegionIndex, REGIONS_IN_GRAPH>,
    region_render_lists: [RegionRenderList; REGIONS_IN_GRAPH],
}

impl StagingRegionRenderLists {
    pub fn touch_region(
        &mut self,
        coord_context: &LocalCoordContext,
        local_section_coord: u8x3,
    ) -> &mut RegionRenderList {
        let local_region_index = LocalRegionIndex::from_local_section(local_section_coord);
        let region_render_list = unsafe {
            self.region_render_lists
                .get_mut(local_region_index.0 as usize)
                .unwrap_unchecked()
        };

        let global_region_coords = coord_context.origin_region_coords
            + (local_section_coord & REGION_MASK >> REGION_COORD_SHIFT).cast::<i32>();

        // we only want to add the region on the first encounter of the region to get
        // the correct render order
        if region_render_list.is_initialized() {
            region_render_list.initialize(global_region_coords);
            self.ordered_region_indices.push(local_region_index);
        } else {
            debug_assert_eq!(global_region_coords, region_render_list.region_coords);
        }

        region_render_list
    }

    pub fn compile_render_lists(&self, results: &mut SortedRegionRenderLists) {
        for local_region_index in self.ordered_region_indices.get_slice() {
            let render_region_list = unsafe {
                self.region_render_lists
                    .get(local_region_index.0 as usize)
                    .unwrap_unchecked()
            };

            // if a region has no sections, skip it. this is a product of making sure the
            // regions are queued in the correct order.
            if !render_region_list.is_empty() {
                results.push(*render_region_list);
            }
        }
    }

    pub fn clear(&mut self) {
        self.ordered_region_indices.clear();

        for render_list in &mut self.region_render_lists {
            render_list.clear();
        }
    }
}

impl InitDefaultInPlace for *mut StagingRegionRenderLists {
    fn init_default_in_place(self) {
        unsafe {
            addr_of_mut!((*self).ordered_region_indices).init_default_in_place();
            addr_of_mut!((*self).region_render_lists).init_default_in_place();
        }
    }
}
