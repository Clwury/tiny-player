use super::*;

pub(super) struct VideoFrameElement {
    pub(super) frame: Arc<RenderImage>,
    pub(super) source_size: RenderSize,
}

impl gpui::Element for VideoFrameElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&gpui::GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut gpui::App,
    ) -> (gpui::LayoutId, Self::RequestLayoutState) {
        let style = gpui::Style {
            size: gpui::Size {
                width: gpui::Length::Definite(gpui::DefiniteLength::Fraction(1.0)),
                height: gpui::Length::Definite(gpui::DefiniteLength::Fraction(1.0)),
            },
            ..Default::default()
        };

        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&gpui::GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut gpui::App,
    ) -> Self::PrepaintState {
    }

    fn paint(
        &mut self,
        _id: Option<&gpui::GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut gpui::App,
    ) {
        let Some(fitted_bounds) = aspect_fit_bounds(bounds, self.source_size) else {
            return;
        };

        let mut corner_radii = gpui::Corners::default();
        if !window.is_maximized() && !window.is_fullscreen() {
            // GPUI's parent overflow mask is rectangular, so the video sprite
            // must also respect the playback viewport's bottom corners.
            let radius = theme::get(cx)
                .radius_lg
                .min(bounds.size.width / 2.0)
                .min(bounds.size.height / 2.0)
                .max(px(0.0));
            // Match paint_image's device-pixel edge snapping, including during
            // fractional-scale resizing. Aspect-fit leaves a gap on one axis;
            // subtracting that gap keeps the sprite inside the window's arc
            // and leaves video corners square when black bars cover the arc.
            let bottom_inset = (window.pixel_snap(bounds.bottom())
                - window.pixel_snap(fitted_bounds.bottom()))
            .max(px(0.0));
            let left_inset = (window.pixel_snap(fitted_bounds.left())
                - window.pixel_snap(bounds.left()))
            .max(px(0.0));
            let right_inset = (window.pixel_snap(bounds.right())
                - window.pixel_snap(fitted_bounds.right()))
            .max(px(0.0));
            corner_radii.bottom_left = (radius - bottom_inset.max(left_inset)).max(px(0.0));
            corner_radii.bottom_right = (radius - bottom_inset.max(right_inset)).max(px(0.0));
        }

        _ = window.paint_image(
            bounds,
            fitted_bounds,
            corner_radii,
            self.frame.clone(),
            0,
            false,
        );
    }
}

impl IntoElement for VideoFrameElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}
