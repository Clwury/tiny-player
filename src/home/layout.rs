//! GPUI adapter for the pure layout policy. One timer per resize burst; frame
//! callbacks retain weak entities and validate the warmup token before mutation.
use gpui::{Context, Window};

use super::{
    HomeContent,
    model::layout::{RESIZE_SETTLE_DEBOUNCE, ResizeTick},
};
use crate::effects::RequestToken;

impl HomeContent {
    pub(super) fn schedule_resize_settle(
        &mut self,
        token: RequestToken,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let task = cx.spawn_in(window, async move |page, cx| {
            let mut wait = RESIZE_SETTLE_DEBOUNCE;
            loop {
                cx.background_executor().timer(wait).await;
                let tick = page.update(cx, |page, cx| {
                    let tick = page.layout.resize_tick(
                        &token,
                        &page.request_identity(),
                        page.controller.route(),
                        page.authentication_error.is_some(),
                        cx.background_executor().now(),
                    );
                    if matches!(tick, ResizeTick::Settled { .. }) {
                        // Commit the final workspace before warming hidden Home.
                        cx.notify();
                    }
                    tick
                });
                match tick {
                    Ok(ResizeTick::Wait(remaining)) => wait = remaining,
                    Ok(ResizeTick::Settled {
                        warmup: Some(token),
                    }) => {
                        // Callbacks run before drawing: nest once to guarantee a
                        // presentation of the settled workspace before warmup.
                        cx.on_next_frame(move |window, _| {
                            window.on_next_frame(move |_, cx| {
                                page.update(cx, |page, cx| {
                                    if page
                                        .layout
                                        .complete_warmup(&token, &page.request_identity())
                                    {
                                        cx.notify();
                                    }
                                })
                                .ok();
                            });
                        });
                        break;
                    }
                    _ => break,
                }
            }
        });
        self.dashboard.resize_settle.replace(task);
    }
}
