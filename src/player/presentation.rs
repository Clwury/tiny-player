//! GPUI image ownership and caching at the application/engine boundary.

use std::sync::Arc;

use gpui::{App, RenderImage, Window};
use image::{Frame, RgbaImage};
use tiny_playback::{BackendSubtitleCue, BgraImage, SharedBgraImage};

/// Transfer a validated video buffer into GPUI without copying its pixels.
pub(super) fn render_image(image: BgraImage) -> Arc<RenderImage> {
    let size = image.size();
    // GPUI's RenderImage expects BGRA bytes, despite image's RGBA container name.
    let pixels = RgbaImage::from_raw(size.width, size.height, image.into_bytes())
        .expect("engine images have validated dimensions and byte lengths");
    Arc::new(RenderImage::new([Frame::new(pixels)]))
}

pub(super) fn defer_drop_frame(frame: Arc<RenderImage>, window: &mut Window) {
    window.on_next_frame(move |window, _| {
        window.on_next_frame(move |window, cx| {
            cx.drop_image(frame, Some(window));
        });
        window.refresh();
    });
    window.refresh();
}

/// Entity release also covers episode replacement and other exits that do not
/// pass through the playback page's back button. Wait for old scenes to retire.
pub(super) fn defer_drop_released_images(images: Vec<Arc<RenderImage>>, cx: &mut App) {
    if images.is_empty() {
        return;
    }
    cx.defer(move |cx| {
        for handle in cx.windows() {
            let _ = handle.update(cx, |_, window, _| {
                for image in &images {
                    defer_drop_frame(image.clone(), window);
                }
            });
        }
    });
}

/// Owns only the images used by the current cue. Repeated cues and overlapping
/// cues share GPUI images; removed images are returned for deferred atlas eviction.
#[derive(Default)]
pub(super) struct SubtitleImages {
    images: Vec<(SharedBgraImage, Arc<RenderImage>)>,
}

impl SubtitleImages {
    pub(super) fn update(&mut self, cue: Option<&BackendSubtitleCue>) -> Vec<Arc<RenderImage>> {
        let mut previous = std::mem::take(&mut self.images);
        for bitmap in cue.into_iter().flat_map(|cue| &cue.bitmaps) {
            if self.get(&bitmap.image).is_some() {
                continue;
            }
            let image =
                if let Some(index) = previous.iter().position(|(key, _)| *key == bitmap.image) {
                    previous.swap_remove(index).1
                } else {
                    let image = bitmap.image.image();
                    let size = image.size();
                    // Subtitle timelines retain shared pixels. Copy once when an
                    // image becomes visible, never on each UI repaint or cue clone.
                    render_image(
                        BgraImage::new(image.bytes().to_vec(), size.width, size.height)
                            .expect("shared engine images are validated"),
                    )
                };
            self.images.push((bitmap.image.clone(), image));
        }
        previous.into_iter().map(|(_, image)| image).collect()
    }

    pub(super) fn get(&self, image: &SharedBgraImage) -> Option<&Arc<RenderImage>> {
        self.images
            .iter()
            .find(|(key, _)| key == image)
            .map(|(_, image)| image)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Context, IntoElement, ParentElement, Render, Styled, div, img};
    use tiny_playback::BackendSubtitleBitmap;

    struct ImagePreview(Option<Arc<RenderImage>>);

    impl Render for ImagePreview {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .children(self.0.clone().map(|image| img(image).size_full()))
        }
    }

    #[gpui::test]
    fn released_view_images_leave_the_atlas_after_two_frame_callbacks(
        cx: &mut gpui::TestAppContext,
    ) {
        let image = render_image(BgraImage::new(vec![1, 2, 3, 255], 1, 1).unwrap());
        let released = std::rc::Rc::new(std::cell::Cell::new(false));
        let (view, cx) = cx.add_window_view(|_, cx| {
            let released = released.clone();
            cx.on_release(move |view: &mut ImagePreview, cx| {
                released.set(true);
                defer_drop_released_images(view.0.take().into_iter().collect(), cx);
            })
            .detach();
            ImagePreview(Some(image.clone()))
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            assert!(window.has_image_atlas_entry(&image));
            window.replace_root(cx, |_, _| ImagePreview(None));
        });
        drop(view);
        cx.run_until_parked();
        // Dropping an entity outside an App update needs an effect cycle before
        // its release listener and deferred window updates run.
        cx.update(|_, _| {});
        cx.update(|window, cx| {
            assert!(
                released.get(),
                "old root view must be released before cleanup callbacks"
            );
            assert!(window.has_image_atlas_entry(&image));
            window.simulate_next_frame(cx);
            assert!(window.has_image_atlas_entry(&image));
            window.simulate_next_frame(cx);
            assert!(!window.has_image_atlas_entry(&image));
        });
    }

    fn bitmap(value: u8) -> SharedBgraImage {
        SharedBgraImage::new(BgraImage::new(vec![value, 2, 3, 128], 1, 1).unwrap())
    }

    fn cue(images: &[SharedBgraImage]) -> BackendSubtitleCue {
        BackendSubtitleCue {
            text: String::new(),
            bitmaps: images
                .iter()
                .map(|image| BackendSubtitleBitmap {
                    image: image.clone(),
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                    canvas_width: 1920,
                    canvas_height: 1080,
                })
                .collect(),
            start_nsecs: 0,
            end_nsecs: 1_000_000_000,
        }
    }

    #[test]
    fn video_adapter_moves_bgra_pixels_without_changing_alpha() {
        let bytes = vec![10, 20, 30, 128, 40, 50, 60, 255];
        let allocation = bytes.as_ptr();
        let image = render_image(BgraImage::new(bytes, 2, 1).unwrap());
        assert_eq!(
            image.as_bytes(0).unwrap(),
            [10, 20, 30, 128, 40, 50, 60, 255]
        );
        assert_eq!(image.as_bytes(0).unwrap().as_ptr(), allocation);
    }

    #[test]
    fn subtitle_images_reuse_identity_and_retire_only_removed_images() {
        let first = bitmap(1);
        let second = bitmap(2);
        let mut images = SubtitleImages::default();
        let initial = cue(&[first.clone(), second.clone(), first.clone()]);
        assert!(images.update(Some(&initial)).is_empty());
        assert_eq!(images.images.len(), 2);
        let first_rendered = images.get(&first).unwrap().clone();
        let second_rendered = images.get(&second).unwrap().clone();
        assert_eq!(first_rendered.as_bytes(0).unwrap(), first.image().bytes());
        assert!(images.update(Some(&initial.clone())).is_empty());
        assert!(Arc::ptr_eq(images.get(&first).unwrap(), &first_rendered));

        let mut next = cue(std::slice::from_ref(&second));
        next.start_nsecs = 10;
        let retired = images.update(Some(&next));
        assert_eq!(retired.len(), 1);
        assert!(Arc::ptr_eq(&retired[0], &first_rendered));
        assert!(Arc::ptr_eq(images.get(&second).unwrap(), &second_rendered));
        assert!(images.get(&first).is_none());

        let retired = images.update(None);
        assert_eq!(retired.len(), 1);
        assert!(Arc::ptr_eq(&retired[0], &second_rendered));
        assert!(images.update(None).is_empty());
    }

    #[test]
    fn equal_pixels_with_new_identity_replace_the_cached_image() {
        let first = bitmap(1);
        let replacement = bitmap(1);
        let mut images = SubtitleImages::default();
        images.update(Some(&cue(std::slice::from_ref(&first))));
        let previous = images.get(&first).unwrap().clone();
        let retired = images.update(Some(&cue(std::slice::from_ref(&replacement))));
        assert_eq!(retired.len(), 1);
        assert_ne!(images.get(&replacement).unwrap().id, previous.id);
    }
}
