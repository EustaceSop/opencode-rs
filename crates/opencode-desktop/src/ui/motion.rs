// Rust build: the motion of the "clay" look. Entrances are short and eased (fade plus a few pixels of travel), and
// only the working pulse loops. With the system's reduced-motion setting, GPUI shows the end state without animating.
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::shimmer::ShimmerText;
use gpui_kit::{Animation, AnimationExt as _, AnyElement, App, ElementId, Hsla, IntoElement, ParentElement as _, SharedString, Styled as _, div, ease_out_quint, pulsating_between, px};
use std::time::Duration;

const ENTER: Duration = Duration::from_millis(240);
const PULSE: Duration = Duration::from_millis(1400);

// Fades `child` in while it rises `rise` pixels into place.
pub fn enter(id: impl Into<ElementId>, rise: f32, child: impl IntoElement) -> AnyElement {
    div()
        .relative()
        .w_full()
        .child(child)
        .with_animation(id, Animation::new(ENTER).with_easing(ease_out_quint()), move |this, t| this.opacity(t).top(px((1.0 - t) * rise)))
        .into_any_element()
}

// A small breathing dot in the accent colour: something is working.
pub fn pulse(id: impl Into<ElementId>, size: f32, cx: &App) -> AnyElement {
    let color: Hsla = cx.theme().primary;
    div()
        .flex_shrink_0()
        .size(px(size))
        .rounded_full()
        .bg(color)
        .with_animation(id, Animation::new(PULSE).repeat().with_easing(pulsating_between(0.3, 1.0)), |this, t| this.opacity(t))
        .into_any_element()
}

// A label with a light sweeping across it while the work it names is in progress.
pub fn shimmer(id: impl Into<ElementId>, label: impl Into<SharedString>) -> AnyElement {
    ShimmerText::new(label).id(id).duration(Duration::from_millis(1800)).into_any_element()
}
