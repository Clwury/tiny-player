use gpui::{
    AnyElement, App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, Pixels, Window,
};

/// Measure a representative card, then build the full row only when it intersects
/// the current viewport. Using prepaint bounds keeps scrolling and native resize
/// correct in the same frame, including GPUI's scroll clamping on height changes.
pub(super) struct VisibleRow<F> {
    measurement: AnyElement,
    render: Option<F>,
}

impl<F> VisibleRow<F> {
    pub(super) fn new(measurement: impl IntoElement, render: F) -> Self {
        Self {
            measurement: measurement.into_any_element(),
            render: Some(render),
        }
    }
}

impl<F: FnOnce(&mut Window, &mut App) -> AnyElement + 'static> IntoElement for VisibleRow<F> {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl<F: FnOnce(&mut Window, &mut App) -> AnyElement + 'static> Element for VisibleRow<F> {
    type RequestLayoutState = ();
    type PrepaintState = Option<AnyElement>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.measurement.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        if !bounds.intersects(&window.content_mask().bounds) {
            return None;
        }

        let mut row = self.render.take().unwrap()(window, cx);
        row.layout_as_root(bounds.size.into(), window, cx);
        row.prepaint_at(bounds.origin, window, cx);
        Some(row)
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        row: &mut Option<AnyElement>,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some(row) = row {
            row.paint(window, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };

    use gpui::{
        Context, InteractiveElement, Modifiers, ParentElement, Render, ScrollHandle,
        StatefulInteractiveElement, Styled, TestAppContext, div, point, px, size,
    };

    use super::*;

    struct Rows {
        built: Rc<RefCell<Vec<usize>>>,
        clicked: Rc<Cell<Option<usize>>>,
        scroll: ScrollHandle,
    }

    impl Render for Rows {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            self.built.borrow_mut().clear();
            div()
                .id("rows")
                .size_full()
                .overflow_y_scroll()
                .track_scroll(&self.scroll)
                .children((0..40).map(|index| {
                    let built = self.built.clone();
                    let clicked = self.clicked.clone();
                    VisibleRow::new(
                        div().w_full().h(px(100.0)),
                        move |_: &mut Window, _: &mut App| {
                            built.borrow_mut().push(index);
                            div()
                                .id(("row", index))
                                .w_full()
                                .h(px(100.0))
                                .on_click(move |_, _, _| clicked.set(Some(index)))
                                .into_any_element()
                        },
                    )
                }))
        }
    }

    #[gpui::test]
    fn resize_builds_only_intersecting_rows_and_preserves_scroll_extent(cx: &mut TestAppContext) {
        let built = Rc::new(RefCell::new(Vec::new()));
        let scroll = ScrollHandle::new();
        let window = cx.open_window(size(px(800.0), px(250.0)), {
            let built = built.clone();
            let scroll = scroll.clone();
            move |_, _| Rows {
                built,
                scroll,
                clicked: Rc::default(),
            }
        });
        cx.run_until_parked();
        assert_eq!(*built.borrow(), vec![0, 1, 2]);
        assert_eq!(scroll.max_offset().y, px(3_750.0));

        // Every native width change forces a refresh, but does not build any
        // additional row bodies. Heights continue to include all 40 rows.
        for width in 801..821 {
            cx.simulate_window_resize(window.into(), size(px(width as f32), px(250.0)));
            cx.run_until_parked();
            assert_eq!(*built.borrow(), vec![0, 1, 2]);
            assert_eq!(scroll.max_offset().y, px(3_750.0));
        }

        scroll.set_offset(point(px(0.0), px(-3_750.0)));
        window.update(cx, |_, window, _| window.refresh()).unwrap();
        cx.run_until_parked();
        assert_eq!(*built.borrow(), vec![37, 38, 39]);

        // Growing at the bottom clamps the offset before testing visibility.
        cx.simulate_window_resize(window.into(), size(px(820.0), px(450.0)));
        cx.run_until_parked();
        assert_eq!(scroll.offset().y, px(-3_550.0));
        assert_eq!(*built.borrow(), vec![35, 36, 37, 38, 39]);

        cx.simulate_window_resize(window.into(), size(px(820.0), px(100.0)));
        cx.run_until_parked();
        assert_eq!(*built.borrow(), vec![35, 36]);
    }

    #[gpui::test]
    fn rows_remain_clickable_at_their_scrolled_position(cx: &mut TestAppContext) {
        let clicked = Rc::new(Cell::new(None));
        let scroll = ScrollHandle::new();
        let (view, cx) = cx.add_window_view({
            let clicked = clicked.clone();
            let scroll = scroll.clone();
            move |_, _| Rows {
                built: Rc::default(),
                clicked,
                scroll,
            }
        });
        cx.simulate_resize(size(px(800.0), px(250.0)));
        cx.run_until_parked();
        scroll.set_offset(point(px(0.0), px(-1_550.0)));
        view.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();

        cx.simulate_click(point(px(50.0), px(25.0)), Modifiers::default());
        assert_eq!(clicked.get(), Some(15));
        cx.simulate_click(point(px(50.0), px(100.0)), Modifiers::default());
        assert_eq!(clicked.get(), Some(16));
    }
}
