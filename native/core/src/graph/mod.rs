use alloc::alloc::alloc;
use alloc::boxed::Box;
use core::alloc::Layout;
use core::mem::swap;
use core::ptr::addr_of_mut;

use core_simd::simd::Which::*;
use core_simd::simd::*;
use local::LocalCoordContext;
use sodium_proc_macros::InitDefaultInPlace;

use self::coord::LocalNodeCoords;
use crate::collections::{ArrayDeque, CInlineVec};
use crate::graph::local::coord::LocalNodeIndex;
use crate::graph::local::*;
use crate::graph::octree::LinearBitOctree;
use crate::graph::visibility::*;
use crate::math::*;
use crate::mem::InitDefaultInPlace;
use crate::region::*;

pub mod flags;
pub mod local;
mod octree;
pub mod visibility;

pub const SECTIONS_IN_GRAPH: usize = 256 * 256 * 256;

pub const MAX_VIEW_DISTANCE: u8 = 127;
pub const MAX_WORLD_HEIGHT: u8 = 254;
pub const BFS_QUEUE_SIZE: usize =
    get_bfs_queue_max_size(MAX_VIEW_DISTANCE, MAX_WORLD_HEIGHT) as usize;
pub type BfsQueue = ArrayDeque<LocalNodeIndex<0>, BFS_QUEUE_SIZE>;
pub type SortedRegionRenderLists = CInlineVec<RegionRenderList, REGIONS_IN_GRAPH>;

pub const fn get_bfs_queue_max_size(section_render_distance: u8, world_height: u8) -> u32 {
    // for the worst case, we will assume the player is in the center of the render
    // distance and world height.
    // for traversal lengths, we don't include the chunk the player is in.

    let max_height_traversal = (world_height.div_ceil(2) - 1) as u32;
    let max_width_traversal = section_render_distance as u32;

    // the 2 accounts for the chunks directly above and below the player
    let mut count = 2;
    let mut layer_index = 1_u32;

    // check if the traversal up and down is restricted by the world height. if so,
    // remove the out-of-bounds layers from the iteration
    if max_height_traversal < max_width_traversal {
        count = 0;
        layer_index = max_width_traversal - max_height_traversal;
    }

    // add rings that are on both the top and bottom.
    // simplification of:
    // while layer_index < max_width_traversal {
    //     count += (layer_index * 8);
    //     layer_index += 1;
    // }
    count += 4 * (max_width_traversal - layer_index) * (max_width_traversal + layer_index - 1);

    // add final, outer-most ring.
    count += max_width_traversal * 4;

    // The frustum can never be wider than 180 degrees, so we can cut the number in
    // half... almost. We have to give a buffer of an extra 5% (arbitrarily
    // selected) because of 2 reasons:
    // 1. Frustum/fog culling is checked after the section has already been added to the queue, not
    //    before.
    // 2. The frustum includes sections that are just barely in view, adding more than half in a
    //    worst-case scenario. This effect becomes more noticeable at smaller render distances.
    // count = (count * 100) / 55;

    count
}

#[derive(InitDefaultInPlace)]
pub struct BfsCachedState {
    incoming_directions: [GraphDirectionSet; SECTIONS_IN_GRAPH],
    staging_render_lists: StagingRegionRenderLists,
    queue_1: BfsQueue,
    queue_2: BfsQueue,
}

impl BfsCachedState {
    pub fn reset(&mut self) {
        self.incoming_directions.fill(GraphDirectionSet::NONE);
        self.staging_render_lists.clear();
        // TODO: are these necessary?
        self.queue_1.reset();
        self.queue_2.reset();
    }
}

#[derive(InitDefaultInPlace)]
pub struct FrustumFogCachedState {
    section_is_visible_bits: LinearBitOctree,
}

impl FrustumFogCachedState {
    pub fn reset(&mut self) {
        self.section_is_visible_bits.clear();
    }
}

#[derive(InitDefaultInPlace)]
pub struct Graph {
    section_visibility_direction_sets: [VisibilityData; SECTIONS_IN_GRAPH],
    // section_flag_sets: [SectionFlagSet; SECTIONS_IN_GRAPH],
    frustum_fog_cached_state: FrustumFogCachedState,
    bfs_cached_state: BfsCachedState,

    results: SortedRegionRenderLists,
}

impl Graph {
    pub fn new_boxed() -> Box<Self> {
        unsafe {
            let uninit = alloc(Layout::new::<Graph>()) as *mut Graph;

            uninit.init_default_in_place();

            Box::from_raw(uninit)
        }
    }

    pub fn cull_and_sort(
        &mut self,
        coord_context: &LocalCoordContext,
        use_occlusion_culling: bool,
    ) -> &SortedRegionRenderLists {
        self.results.clear();

        self.frustum_and_fog_cull(coord_context);
        self.bfs_and_occlusion_cull(coord_context, use_occlusion_culling);

        self.bfs_cached_state
            .staging_render_lists
            .compile_render_lists(&mut self.results);

        // this will make sure nothing tries to use it after culling, and it should be
        // clean for the next invocation of this method
        self.bfs_cached_state.reset();
        self.frustum_fog_cached_state.reset();

        &self.results
    }

    fn frustum_and_fog_cull(&mut self, coord_context: &LocalCoordContext) {
        // this could go more linearly in memory, but we probably have good enough
        // locality inside the level 3 nodes
        let mut level_3_index_x_incr = coord_context.iter_start_index;
        for _x_offset in 0..coord_context.level_3_node_iter_counts.x() {
            let mut level_3_index_xy_incr = level_3_index_x_incr;
            for _y_offset in 0..coord_context.level_3_node_iter_counts.y() {
                let mut level_3_index_xyz_incr = level_3_index_xy_incr;
                for _z_offset in 0..coord_context.level_3_node_iter_counts.z() {
                    self.check_node(level_3_index_xyz_incr, coord_context);

                    level_3_index_xyz_incr = level_3_index_xyz_incr.inc_z();
                }
                level_3_index_xy_incr = level_3_index_xy_incr.inc_y();
            }
            level_3_index_x_incr = level_3_index_x_incr.inc_x();
        }
    }

    fn check_node<const LEVEL: u8>(
        &mut self,
        index: LocalNodeIndex<LEVEL>,
        coord_context: &LocalCoordContext,
    ) {
        match coord_context.test_node(index) {
            BoundsCheckResult::Outside => {}
            BoundsCheckResult::Inside => {
                self.frustum_fog_cached_state
                    .section_is_visible_bits
                    .set(index, true);
            }
            BoundsCheckResult::Partial => match LEVEL {
                3 => {
                    for lower_node_index in index.iter_lower_nodes::<2>() {
                        self.check_node(lower_node_index, coord_context);
                    }
                }
                2 => {
                    for lower_node_index in index.iter_lower_nodes::<1>() {
                        self.check_node(lower_node_index, coord_context);
                    }
                }
                1 => {
                    for lower_node_index in index.iter_lower_nodes::<0>() {
                        self.check_node(lower_node_index, coord_context);
                    }
                }
                0 => {
                    self.frustum_fog_cached_state
                        .section_is_visible_bits
                        .set(index, true);
                }
                _ => unreachable!("Invalid node level: {}", LEVEL),
            },
        }
    }

    fn bfs_and_occlusion_cull(
        &mut self,
        coord_context: &LocalCoordContext,
        use_occlusion_culling: bool,
    ) {
        let directions_modifier = if use_occlusion_culling {
            GraphDirectionSet::NONE
        } else {
            GraphDirectionSet::ALL
        };

        let mut read_queue_ref = &mut self.bfs_cached_state.queue_1;
        let mut write_queue_ref = &mut self.bfs_cached_state.queue_2;

        // Manually add the secton the camera is in as the section to search from
        let initial_node_index = coord_context.camera_section_index;
        read_queue_ref.push(initial_node_index);

        // All incoming directions are set for the first section to make sure we try all
        // of its outgoing directions.
        initial_node_index
            .index_array_unchecked_mut(&mut self.bfs_cached_state.incoming_directions)
            .add_all(GraphDirectionSet::ALL);

        let mut finished = false;
        // this finishes when the read queue is completely empty.
        while !finished {
            finished = true;

            while let Some(&local_section_index) = read_queue_ref.pop() {
                finished = false;

                let local_section_coords = local_section_index.unpack();

                // we need to touch the region before checking if the node is visible, because
                // skipping sections can cause the region order to become incorrect
                let region_render_list = self
                    .bfs_cached_state
                    .staging_render_lists
                    .touch_region(coord_context, local_section_coords);

                if !self
                    .frustum_fog_cached_state
                    .section_is_visible_bits
                    .get(local_section_index)
                {
                    // skip node
                    continue;
                }

                // let section_flags =
                // *node_index.index_array_unchecked(&self.section_flag_sets);
                region_render_list.add_section(local_section_coords);

                // use incoming directions to determine outgoing directions, given the
                // visibility bits set
                let section_incoming_directions = *local_section_index
                    .index_array_unchecked(&self.bfs_cached_state.incoming_directions);

                let mut section_outgoing_directions = local_section_index
                    .index_array_unchecked(&self.section_visibility_direction_sets)
                    .get_outgoing_directions(section_incoming_directions);
                section_outgoing_directions.add_all(directions_modifier);
                section_outgoing_directions &=
                    coord_context.get_valid_directions(local_section_coords);

                // use the outgoing directions to get the neighbors that could possibly be
                // enqueued
                let section_neighbor_indices = local_section_index.get_all_neighbors();

                for direction in section_outgoing_directions {
                    let neighbor_index = section_neighbor_indices.get(direction);

                    // the outgoing direction for the current node is the incoming direction for the
                    // neighbor
                    let current_incoming_direction = direction.opposite();

                    let neighbor_incoming_directions = neighbor_index
                        .index_array_unchecked_mut(&mut self.bfs_cached_state.incoming_directions);

                    // enqueue only if the node has not yet been enqueued, avoiding duplicates
                    let should_enqueue = neighbor_incoming_directions.is_empty();

                    neighbor_incoming_directions.add(current_incoming_direction);

                    write_queue_ref.push_conditionally(neighbor_index, should_enqueue);
                }
            }

            // we need to reset the read queue because, even though there are no elements left, we
            // want to set the head and tail pointers to the start of the array.
            read_queue_ref.reset();
            swap(&mut read_queue_ref, &mut write_queue_ref);
        }
    }

    pub fn set_section(&mut self, section_coord: i32x3, visibility_data: VisibilityData) {
        let local_coord = LocalNodeCoords::from_raw(
            section_coord.cast::<u8>() + u8x3::from_xyz(0, LocalCoordContext::Y_ADD_SECTIONS, 0),
        );
        let index = LocalNodeIndex::<0>::pack(local_coord);

        // *index.index_array_unchecked_mut(&mut self.section_flag_sets) = flags;
        *index.index_array_unchecked_mut(&mut self.section_visibility_direction_sets) =
            visibility_data;
    }

    pub fn remove_section(&mut self, section_coord: i32x3) {
        self.set_section(section_coord, Default::default());
    }
}
