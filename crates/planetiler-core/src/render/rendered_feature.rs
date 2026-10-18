//! Port of `render/RenderedFeature.java`.

use std::sync::Arc;

use crate::geo::tile_coord::TileCoord;
use crate::vector_tile;

/// Information used to limit features or assign a "rank" for features in a certain area of the tile.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Group {
    /// ID of the group that features live in.
    pub group: i64,
    /// Maximum rank within `group` in a tile that this feature should be included at, for example if this is the 4th
    /// feature in a group with lowest sort-key then the feature is included if `limit <= 4`.
    pub limit: i32,
}

/// An encoded vector tile feature on a tile with an extra `sort_key` and `group` that define its placement in the
/// eventual output tile.
///
/// The vector tile feature is shared (`Arc`) so that identical features emitted to many tiles (filled ocean tiles)
/// can be detected with [`Arc::ptr_eq`] and encoded only once.
#[derive(Clone, Debug, PartialEq)]
pub struct RenderedFeature {
    /// The tile this feature will live in.
    pub tile: TileCoord,
    /// The encoded vector tile feature.
    pub vector_tile_feature: Arc<vector_tile::Feature>,
    /// Ordering of features in the output tile.
    pub sort_key: i32,
    /// If present, a group ID and limit that is used to limit features in a certain area of a tile.
    pub group: Option<Group>,
}

impl RenderedFeature {
    pub fn new(
        tile: TileCoord,
        vector_tile_feature: impl Into<Arc<vector_tile::Feature>>,
        sort_key: i32,
        group: Option<Group>,
    ) -> RenderedFeature {
        RenderedFeature { tile, vector_tile_feature: vector_tile_feature.into(), sort_key, group }
    }
}
