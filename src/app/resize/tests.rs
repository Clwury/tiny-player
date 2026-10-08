use super::*;
use gpui::{Point, Tiling, size};

fn edge_at(
    regions: &[(ResizeEdge, Bounds<Pixels>)],
    position: Point<Pixels>,
) -> Option<ResizeEdge> {
    let hits: Vec<_> = regions
        .iter()
        .filter(|(_, bounds)| bounds.contains(&position))
        .collect();
    assert!(
        hits.len() <= 1,
        "overlapping resize regions at {position:?}"
    );
    hits.first().map(|(edge, _)| *edge)
}

#[test]
fn resize_ring_covers_the_border_and_exterior_without_entering_content() {
    let bounds = Bounds::new(point(px(3.0), px(5.0)), size(px(100.0), px(80.0)));
    let width = WINDOW_RESIZE_OUTSET + px(1.0);
    let regions = resize_regions(bounds, Edges::all(width));
    for (position, expected) in [
        ((53.0, 5.5), ResizeEdge::Top),
        ((53.0, 13.5), ResizeEdge::Top),
        ((102.5, 45.0), ResizeEdge::Right),
        ((94.5, 45.0), ResizeEdge::Right),
        ((53.0, 84.5), ResizeEdge::Bottom),
        ((53.0, 76.5), ResizeEdge::Bottom),
        ((3.5, 45.0), ResizeEdge::Left),
        ((11.5, 45.0), ResizeEdge::Left),
        ((16.0, 5.5), ResizeEdge::TopLeft),
        ((3.5, 18.0), ResizeEdge::TopLeft),
        ((90.0, 5.5), ResizeEdge::TopRight),
        ((102.5, 18.0), ResizeEdge::TopRight),
        ((16.0, 84.5), ResizeEdge::BottomLeft),
        ((3.5, 72.0), ResizeEdge::BottomLeft),
        ((90.0, 84.5), ResizeEdge::BottomRight),
        ((102.5, 72.0), ResizeEdge::BottomRight),
    ] {
        assert_eq!(
            edge_at(&regions, point(px(position.0), px(position.1))),
            Some(expected)
        );
    }

    let content = bounds.inset(width);
    for y in 0..80 {
        for x in 0..100 {
            let position = bounds.origin + point(px(x as f32 + 0.5), px(y as f32 + 0.5));
            assert_eq!(
                edge_at(&regions, position).is_none(),
                content.contains(&position)
            );
        }
    }
}

#[test]
fn tiled_edges_have_no_resize_bands_or_diagonal_corners() {
    let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(100.0), px(80.0)));
    for bits in 0..16 {
        let tiling = Tiling {
            top: bits & 1 != 0,
            right: bits & 2 != 0,
            bottom: bits & 4 != 0,
            left: bits & 8 != 0,
        };
        let edges =
            crate::app::window::untiled_border_widths(tiling, WINDOW_RESIZE_OUTSET + px(1.0));
        for (edge, _) in resize_regions(bounds, edges) {
            let enabled = match edge {
                ResizeEdge::Top => !tiling.top,
                ResizeEdge::Right => !tiling.right,
                ResizeEdge::Bottom => !tiling.bottom,
                ResizeEdge::Left => !tiling.left,
                ResizeEdge::TopLeft => !tiling.top && !tiling.left,
                ResizeEdge::TopRight => !tiling.top && !tiling.right,
                ResizeEdge::BottomLeft => !tiling.bottom && !tiling.left,
                ResizeEdge::BottomRight => !tiling.bottom && !tiling.right,
            };
            assert!(enabled, "{edge:?} on {tiling:?}");
        }
        for (position, tiled) in [
            (point(px(50.0), px(0.5)), tiling.top),
            (point(px(99.5), px(40.0)), tiling.right),
            (point(px(50.0), px(79.5)), tiling.bottom),
            (point(px(0.5), px(40.0)), tiling.left),
        ] {
            assert_eq!(
                edge_at(&resize_regions(bounds, edges), position).is_none(),
                tiled
            );
        }
    }
}
