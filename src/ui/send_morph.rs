//! The split reply turning into its own card in the conversation when it is
//! sent.
//!
//! A stage covers the reading pane and draws two stills of it: the pane as
//! it was when Send was pressed, and the pane once the composer has gone and
//! the reply's card is in the thread. The editor card is carried to where
//! the message card now sits and becomes it, the conversation slides to its
//! new place around it, and the composer's buttons fade. Everything that
//! moves is a texture on one frame clock: nothing is laid out mid-flight,
//! the web view and GTK cannot drift apart, and the last frame is the
//! pane's own pixels, so lifting the stage changes nothing on screen.

use std::cell::{Cell, RefCell};

use adw::prelude::*;
use gtk::{gdk, glib, graphene, gsk, subclass::prelude::*};

/// How long the editor card takes to reach its place in the conversation.
const MOTION_MS: f64 = 480.0;
/// How long the composer's buttons and fields take to fade, from Send.
const CHROME_MS: f64 = 200.0;
/// How long to wait for the conversation to be ready before giving up and
/// fading the stage instead.
const READY_TIMEOUT_MS: u64 = 1500;
/// How long the reply's card may keep moving in the document before the
/// stage lands on it as it stands.
const SETTLE_MS: u64 = 450;
/// The editor card's corners (libadwaita's `.card`) and the conversation
/// card's (`.hylki-msg`): the same 12px, kept apart in case either moves.
const EDITOR_RADIUS: f32 = 12.0;
const CARD_RADIUS: f32 = 12.0;

/// One conversation card as the reader document lays it out, in stage
/// coordinates.
#[derive(Clone, Debug)]
pub struct Card {
    pub key: String,
    pub rect: graphene::Rect,
    /// The height of its header line, where the sender and date sit.
    pub head: f32,
    /// Where its first line of text begins, and the size it is set in.
    pub text: Option<(graphene::Point, f32)>,
}

/// The pane at the moment Send was pressed.
pub struct Before {
    pub still: gdk::Texture,
    /// The split slot the composer fills.
    pub composer: graphene::Rect,
    /// The editor card inside it.
    pub editor: graphene::Rect,
    /// The reader beside the composer, and its web view.
    pub reader: graphene::Rect,
    pub view: graphene::Rect,
    /// The reader's header bar, when it was showing (a split above the
    /// reader hides it).
    pub header: Option<graphene::Rect>,
    /// What the window paints behind the pane, where the stills are
    /// see-through: the pane's own widgets leave their background to it.
    pub ground: Option<gdk::RGBA>,
    /// The conversation's cards as the document had them, once measured.
    pub cards: Vec<Card>,
    /// Where the first line written in the editor begins, and its size.
    pub text: Option<(graphene::Point, f32)>,
}

/// The pane with the composer gone and the reply in the conversation.
pub struct After {
    pub still: gdk::Texture,
    pub reader: graphene::Rect,
    pub view: graphene::Rect,
    pub header: Option<graphene::Rect>,
    /// Every card, in document order.
    pub cards: Vec<Card>,
    /// The reply's own card among them.
    pub arrival: String,
}

/// Everything the frames are drawn from, worked out once when the motion
/// starts.
pub struct Scene {
    after: After,
    /// The reply's card, cut to what the web view shows of it.
    card: graphene::Rect,
    /// Its corner radii: none on an edge the web view cuts off.
    card_radius: (f32, f32),
    /// Where the first line of text starts in the editor card and in the
    /// message card, from each card's corner. The two are carried to meet,
    /// so the words written stay put while the card changes around them.
    editor_text: (f32, f32),
    card_text: (f32, f32),
    /// How much larger the card sets the words than the editor did.
    text_ratio: f32,
    /// How far the cards above and below the reply move.
    shift_above: f32,
    shift_below: f32,
    /// The composer's own ground, and the window's behind the pane.
    composer_ground: gdk::RGBA,
    ground: gdk::RGBA,
    page: gdk::RGBA,
    editor_ground: gdk::RGBA,
    card_ground: gdk::RGBA,
    /// The pieces that fade, cut out of the after still.
    header_cut: Option<Cut>,
    card_cut: Option<Cut>,
}

/// A piece of a still in a texture of its own, and where it sits. Faded on
/// its own, a piece costs the GPU only its own size; faded as a clip of the
/// whole still, it costs the whole pane, every frame.
pub struct Cut {
    tex: gdk::Texture,
    at: graphene::Rect,
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct SendMorph {
        pub before: RefCell<Option<Before>>,
        pub scene: RefCell<Option<Scene>>,
        /// The texture scale: device pixels per logical pixel.
        pub scale: Cell<f32>,
        /// Frame times (µs) the chrome fade and the motion started at; 0
        /// until the first frame after they were asked for.
        pub chrome_start: Cell<i64>,
        pub motion_start: Cell<i64>,
        /// Frames the landed scene has been drawn at rest, before the clock
        /// starts.
        pub rest_frames: Cell<u32>,
        pub now: Cell<i64>,
        pub tick: RefCell<Option<gtk::TickCallbackId>>,
        /// The stage's size when it went up: a resize mid-flight ends it.
        pub size: Cell<(i32, i32)>,
        pub fading: RefCell<Option<adw::TimedAnimation>>,
        /// The composer's ground, which the stage shows where it stood.
        pub base: Cell<Option<gdk::RGBA>>,
        /// The composer without its editor card, and the editor card.
        pub chrome_cut: RefCell<Option<Cut>>,
        pub editor_cut: RefCell<Option<Cut>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for SendMorph {
        const NAME: &'static str = "HylkiSendMorph";
        type Type = super::SendMorph;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for SendMorph {
        fn dispose(&self) {
            if let Some(id) = self.tick.take() {
                id.remove();
            }
        }
    }

    impl WidgetImpl for SendMorph {
        fn size_allocate(&self, width: i32, height: i32, _baseline: i32) {
            let (w, h) = self.size.get();
            if self.before.borrow().is_some() && (w, h) != (width, height) && w > 0 {
                // The stills no longer fit the pane: let it show itself.
                let obj = self.obj().clone();
                glib::idle_add_local_once(move || obj.finish());
            }
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            self.obj().draw(snapshot);
        }
    }
}

glib::wrapper! {
    pub struct SendMorph(ObjectSubclass<imp::SendMorph>)
        @extends gtk::Widget;
}

impl Default for SendMorph {
    fn default() -> Self {
        Self::new()
    }
}

impl SendMorph {
    pub fn new() -> Self {
        let obj: Self = glib::Object::new();
        obj.set_visible(false);
        obj.set_can_target(false);
        obj.set_can_focus(false);
        obj
    }

    /// Whether a morph is on screen.
    pub fn running(&self) -> bool {
        self.imp().before.borrow().is_some()
    }

    /// Take the pane as it looks now, the still the morph starts from. The
    /// stage goes up over it with [`Self::show`].
    pub fn begin(&self, before: Before, scale: f32) {
        let imp = self.imp();
        if let Some(a) = imp.fading.take() {
            a.skip();
        }
        imp.scale.set(scale);
        // The stage is still hidden, so the pane's size is the one it will
        // be given.
        imp.size.set(self.pane().map_or((0, 0), |p| (p.width(), p.height())));
        let e = &before.editor;
        imp.base.set(self.native().and_then(|n| n.renderer()).and_then(|r| {
            sample(&r, &before.still, scale, (before.composer.x() + 4.0).max(e.x() - 6.0), e.y() + e.height() / 2.0)
        }));
        if let Some(r) = self.native().and_then(|n| n.renderer()) {
            imp.chrome_cut.replace(cut(&r, &before.still, scale, &before.composer, Some(&inflate(e, 4.0))));
            imp.editor_cut.replace(cut(&r, &before.still, scale, e, None));
        }
        imp.before.replace(Some(before));
        imp.scene.replace(None);
        imp.chrome_start.set(0);
        imp.motion_start.set(0);
        imp.now.set(0);
    }

    /// Put the stage up, over a pane about to change underneath it. Until
    /// now the real composer has stayed on screen: the stills are complete
    /// only once the web views have answered.
    pub fn show(&self) {
        if !self.running() {
            return;
        }
        self.set_opacity(1.0);
        self.set_visible(true);
        // Clicks during the motion would land on a pane that is not where
        // it looks to be.
        self.set_can_target(true);
        self.ensure_ticking();
        self.queue_draw();
    }

    /// Where the conversation's cards were when Send was pressed.
    pub fn set_before_cards(&self, cards: Vec<Card>) {
        if let Some(b) = self.imp().before.borrow_mut().as_mut() {
            b.cards = cards;
        }
    }

    /// The pane is ready in its new state: move everything there.
    pub fn land(&self, after: After) {
        let imp = self.imp();
        let scene = match imp.before.borrow().as_ref() {
            Some(before) => self.scene_for(before, after),
            None => return,
        };
        let Some(scene) = scene else {
            tracing::info!(target: "hylki::morph", "send morph: the stills could not be laid out");
            self.abort();
            return;
        };
        imp.scene.replace(Some(scene));
        imp.rest_frames.set(0);
        imp.motion_start.set(0);
        // The stage hides the pane completely from here to the last frame:
        // GTK need not draw it under the stage meanwhile. Its web view has
        // painted what the after still shows; nothing waits on it now.
        self.set_pane_drawn(false);
        self.ensure_ticking();
        self.queue_draw();
    }

    /// The pane under the stage: the overlay's main child.
    fn pane(&self) -> Option<gtk::Widget> {
        self.parent().and_downcast::<gtk::Overlay>().and_then(|o| o.child())
    }

    fn set_pane_drawn(&self, drawn: bool) {
        if let Some(pane) = self.pane() {
            pane.set_opacity(if drawn { 1.0 } else { 0.0 });
        }
    }

    /// Give up on the motion: the pane underneath is already in its new
    /// state, so the stage fades away over it.
    pub fn abort(&self) {
        let imp = self.imp();
        if imp.before.borrow().is_none() || imp.fading.borrow().is_some() {
            return;
        }
        if let Some(id) = imp.tick.take() {
            id.remove();
        }
        self.set_can_target(false);
        self.set_pane_drawn(true);
        let target = adw::CallbackAnimationTarget::new({
            let obj = self.clone();
            move |v| obj.set_opacity(v)
        });
        let anim = adw::TimedAnimation::new(self, self.opacity(), 0.0, 160, target);
        anim.set_easing(adw::Easing::EaseOutCubic);
        let obj = self.clone();
        anim.connect_done(move |_| {
            obj.imp().fading.take();
            obj.finish();
        });
        imp.fading.replace(Some(anim.clone()));
        anim.play();
    }

    /// Lift the stage and let go of the stills.
    pub fn finish(&self) {
        let imp = self.imp();
        if let Some(id) = imp.tick.take() {
            id.remove();
        }
        if let Some(a) = imp.fading.take() {
            a.pause();
        }
        if imp.before.take().is_some() {
            record_final(self);
        }
        imp.chrome_cut.replace(None);
        imp.editor_cut.replace(None);
        imp.scene.replace(None);
        self.set_pane_drawn(true);
        self.set_visible(false);
        self.set_can_target(false);
        self.set_opacity(1.0);
    }

    fn ensure_ticking(&self) {
        let imp = self.imp();
        if imp.tick.borrow().is_some() {
            return;
        }
        let id = self.add_tick_callback(|obj, clock| {
            let imp = obj.imp();
            let now = clock.frame_time();
            let interval = now - imp.now.get();
            imp.now.set(now);
            if imp.chrome_start.get() == 0 {
                imp.chrome_start.set(now);
            }
            if imp.scene.borrow().is_some() && imp.motion_start.get() == 0 {
                // The first frames of the scene can be slow to draw (its
                // stills going up to the GPU, its shaders being built). They
                // are drawn at rest, and the card sets off once a frame has
                // come in on time: a slow frame mid-flight would be a jump.
                let rest = imp.rest_frames.get();
                if (rest >= 1 && interval < 22_000) || rest >= 5 {
                    imp.motion_start.set(now);
                } else {
                    imp.rest_frames.set(rest + 1);
                }
            }
            obj.queue_draw();
            record_frame(obj);
            if imp.scene.borrow().is_some() && obj.motion_progress() >= 1.0 {
                // The frame just queued is the last: the pane underneath
                // looks the same, so the stage goes after it is drawn.
                imp.tick.take();
                let obj = obj.clone();
                glib::idle_add_local_once(move || obj.finish());
                return glib::ControlFlow::Break;
            }
            glib::ControlFlow::Continue
        });
        imp.tick.replace(Some(id));
    }

    fn elapsed_ms(&self, start: i64) -> f64 {
        if start == 0 {
            return 0.0;
        }
        (self.imp().now.get() - start).max(0) as f64 / 1000.0 / slow_motion()
    }

    fn motion_progress(&self) -> f64 {
        (self.elapsed_ms(self.imp().motion_start.get()) / MOTION_MS).clamp(0.0, 1.0)
    }

    fn chrome_progress(&self) -> f64 {
        (self.elapsed_ms(self.imp().chrome_start.get()) / CHROME_MS).clamp(0.0, 1.0)
    }

    /// Work out where everything goes, from the two stills and the card
    /// positions measured in each.
    fn scene_for(&self, before: &Before, after: After) -> Option<Scene> {
        let at = after.cards.iter().position(|c| c.key == after.arrival)?;
        let arrival = after.cards[at].clone();
        let view = &after.view;
        let top = arrival.rect.y().max(view.y());
        let bottom = (arrival.rect.y() + arrival.rect.height()).min(view.y() + view.height());
        if bottom - top < 8.0 {
            // The card landed out of sight: nothing to carry the editor to.
            tracing::info!(target: "hylki::morph", "send morph: the reply's card is out of sight");
            return None;
        }
        let card = graphene::Rect::new(arrival.rect.x(), top, arrival.rect.width(), bottom - top);
        let card_radius = (
            if arrival.rect.y() >= view.y() - 0.5 { CARD_RADIUS } else { 0.0 },
            if arrival.rect.y() + arrival.rect.height() <= view.y() + view.height() + 0.5 {
                CARD_RADIUS
            } else {
                0.0
            },
        );
        // The cards on either side of the reply each move as one: the
        // shift of any card on that side that both documents have.
        let shift_of = |cards: &[Card]| -> Option<f32> {
            cards.iter().find_map(|c| {
                before.cards.iter().find(|o| o.key == c.key).map(|o| c.rect.y() - o.rect.y())
            })
        };
        let above: Vec<Card> = after.cards[..at].iter().rev().cloned().collect();
        let below: Vec<Card> = after.cards[at + 1..].to_vec();
        let shift_above = shift_of(&above);
        let shift_below = shift_of(&below);
        // A side with no card in both moves with the other side, or failing
        // that with the top of the view.
        let shift_above = shift_above.or(shift_below).unwrap_or(view.y() - before.view.y());
        let shift_below = shift_below.unwrap_or(shift_above);
        let scale = self.imp().scale.get();
        let renderer = self.native().and_then(|n| n.renderer())?;
        let sample = |tex: &gdk::Texture, x: f32, y: f32| sample(&renderer, tex, scale, x, y);
        let e = &before.editor;
        let composer_ground = self.imp().base.get()?;
        let ground = before.ground.unwrap_or(composer_ground);
        let page = sample(&after.still, view.x() + 3.0, view.y() + view.height() / 2.0)?;
        let editor_ground = sample(&before.still, e.x() + 6.0, e.y() + e.height() - 6.0)?;
        let card_ground = sample(&after.still, card.x() + 6.0, card.y() + card.height() - 6.0)?;
        let header_cut = after.header.and_then(|hb| cut(&renderer, &after.still, scale, &hb, None));
        let card_cut = cut(&renderer, &after.still, scale, &card, None);
        // Without a measured line (an empty body), the cards' tops meet.
        let (editor_text, card_text, text_ratio) = match (before.text, arrival.text) {
            (Some((et, es)), Some((ct, cs))) => (
                (et.x() - e.x(), et.y() - e.y()),
                (ct.x() - card.x(), ct.y() - card.y()),
                if es > 0.0 && cs > 0.0 { (cs / es).clamp(0.5, 2.0) } else { 1.0 },
            ),
            _ => ((0.0, 0.0), (0.0, arrival.head), 1.0),
        };
        Some(Scene {
            editor_text,
            card_text,
            text_ratio,
            card,
            card_radius,
            shift_above,
            shift_below,
            composer_ground,
            ground,
            page,
            editor_ground,
            card_ground,
            header_cut,
            card_cut,
            after,
        })
    }

    fn draw(&self, s: &gtk::Snapshot) {
        let imp = self.imp();
        // At rest before the clock starts, the scene is drawn a hair past
        // its start, so every way the flight draws is already in use.
        let resting = imp.scene.borrow().is_some() && imp.motion_start.get() == 0;
        let t = if resting { 0.001 } else { self.motion_progress() };
        self.draw_at(s, t, self.chrome_progress());
    }

    /// Draw the stage `t` of the way through the motion, the chrome fade
    /// `c` of the way through its own.
    fn draw_at(&self, s: &gtk::Snapshot, t: f64, c: f64) {
        let imp = self.imp();
        let before = imp.before.borrow();
        let Some(before) = before.as_ref() else { return };
        let scale = imp.scale.get();
        let (w, h) = (self.width() as f32, self.height() as f32);
        let full = graphene::Rect::new(0.0, 0.0, w, h);
        let chrome = ease_out(c) as f32;
        let editor = &before.editor;
        let cutout = inflate(editor, 4.0);
        let scene = imp.scene.borrow();
        let Some(scene) = scene.as_ref() else {
            // Waiting for the conversation: the pane as it was, the
            // composer's buttons and fields already on their way out.
            if let Some(ground) = before.ground {
                s.append_color(&ground, &full);
            }
            draw_still(s, &before.still, scale, &full, 0.0, 0.0);
            if let (true, Some(base)) = (chrome > 0.0, imp.base.get()) {
                let mut c = base;
                c.set_alpha(chrome);
                for r in around(&before.composer, &cutout) {
                    s.append_color(&c, &r);
                }
            }
            // The editor card lifts off the composer while it waits.
            let rounded = rounded_rect(editor, EDITOR_RADIUS, EDITOR_RADIUS);
            shadow(s, &rounded, chrome);
            if let Some(c) = imp.editor_cut.borrow().as_ref() {
                s.push_rounded_clip(&rounded);
                draw_cut(s, c, scale, 0.0, 0.0);
                s.pop();
            }
            return;
        };
        let after = &scene.after;
        s.append_color(&scene.ground, &full);
        if t >= 1.0 {
            // The last frame is the pane itself.
            draw_still(s, &after.still, scale, &full, 0.0, 0.0);
            return;
        }
        let e = ease(t) as f32;
        let lerp = |a: f32, b: f32| a + (b - a) * e;

        // Where the composer stood, its ground giving way to the window's.
        {
            let mut c = scene.composer_ground;
            c.set_alpha(1.0 - e);
            s.append_color(&c, &before.composer);
        }

        // The reader's header bar: back in from the top edge if the split
        // had hidden it, as the split's own close brings it back.
        let view = lerp_rect(&before.view, &after.view, e);
        let head_top = match (before.header, after.header) {
            (None, Some(hb)) => {
                if let Some(c) = scene.header_cut.as_ref() {
                    let dy = -(1.0 - e) * (hb.y() + hb.height());
                    s.push_opacity(smoothstep(0.0, 0.6, t));
                    draw_cut(s, c, scale, 0.0, dy);
                    s.pop();
                }
                hb.y() + hb.height()
            }
            (_, Some(hb)) => {
                s.push_clip(&hb);
                draw_still(s, &after.still, scale, &full, 0.0, 0.0);
                s.pop();
                hb.y() + hb.height()
            }
            (_, None) => after.reader.y(),
        };
        // The subject and everything else above the conversation, rising
        // (or sinking) with its top edge.
        {
            let dy = (1.0 - e) * (before.view.y() - after.view.y());
            let top = head_top + dy;
            let clip = graphene::Rect::new(after.reader.x(), top, after.reader.width(), (view.y() - top).max(0.0));
            s.push_clip(&clip);
            draw_still(s, &after.still, scale, &full, 0.0, dy);
            s.pop();
        }
        // And what sits under the conversation (the attachment drawer).
        {
            let bottom_after = after.view.y() + after.view.height();
            let dy = (1.0 - e) * ((before.view.y() + before.view.height()) - bottom_after);
            let top = view.y() + view.height();
            let bottom = after.reader.y() + after.reader.height() + dy;
            let clip = graphene::Rect::new(after.reader.x(), top, after.reader.width(), (bottom - top).max(0.0));
            s.push_clip(&clip);
            draw_still(s, &after.still, scale, &full, 0.0, dy);
            s.pop();
        }

        // The morphing card's frame, which the old conversation keeps clear of.
        let frame = lerp_rect(editor, &scene.card, e);
        let frame_bottom = frame.y() + frame.height();

        // The conversation, sliding to its new place: what the reader showed
        // before, with the finished document over it wherever the two meet,
        // so whatever comes into view on the way is already the new one.
        s.push_clip(&view);
        s.append_color(&scene.page, &view);
        let card = &scene.card;
        let (sa, sb) = (scene.shift_above, scene.shift_below);
        let old_split_above = card.y() - sa;
        let old_split_below = card.y() + card.height() - sb;
        {
            let dy = e * sa;
            let top = before.view.y() + dy;
            let bottom = (old_split_above + dy).min(frame.y());
            if bottom > top {
                s.push_clip(&graphene::Rect::new(view.x(), top, view.width(), bottom - top));
                draw_still(s, &before.still, scale, &full, 0.0, dy);
                s.pop();
            }
        }
        {
            let dy = e * sb;
            let top = (old_split_below + dy).max(frame_bottom);
            let bottom = before.view.y() + before.view.height() + dy;
            if bottom > top {
                s.push_clip(&graphene::Rect::new(view.x(), top, view.width(), bottom - top));
                draw_still(s, &before.still, scale, &full, 0.0, dy);
                s.pop();
            }
        }
        {
            let dy = (e - 1.0) * sa;
            let top = after.view.y() + dy;
            let bottom = card.y() + dy;
            if bottom > top {
                s.push_clip(&graphene::Rect::new(view.x(), top, view.width(), bottom - top));
                draw_still(s, &after.still, scale, &full, 0.0, dy);
                s.pop();
            }
        }
        {
            let dy = (e - 1.0) * sb;
            let top = card.y() + card.height() + dy;
            let bottom = after.view.y() + after.view.height() + dy;
            if bottom > top {
                s.push_clip(&graphene::Rect::new(view.x(), top, view.width(), bottom - top));
                draw_still(s, &after.still, scale, &full, 0.0, dy);
                s.pop();
            }
        }
        s.pop();

        // The composer's buttons and fields, fading where they stood.
        if let (true, Some(c)) = (chrome < 1.0, imp.chrome_cut.borrow().as_ref()) {
            s.push_opacity((1.0 - chrome) as f64);
            draw_cut(s, c, scale, 0.0, 0.0);
            s.pop();
        }

        // The editor card becoming the message card. It lifts off the
        // composer, its shadow growing and settling again as it lands, and
        // its frame takes the message card's place and size.
        let (rt, rb) = scene.card_radius;
        let top_r = lerp(EDITOR_RADIUS, rt);
        let bottom_r = lerp(EDITOR_RADIUS, rb);
        let rounded = rounded_rect(&frame, top_r, bottom_r);
        // Lifted while waiting, it stays up for the first half of the
        // flight and settles as it lands.
        shadow(s, &rounded, chrome * (1.0 - smoothstep(0.45, 1.0, t) as f32));
        s.push_rounded_clip(&rounded);
        s.append_color(&mix(&scene.editor_ground, &scene.card_ground, e), &frame);
        // The same words lie on top of each other, so the card's own
        // picture comes up over the editor's as the editor's goes: the text
        // never thins out on the way. Brief, because the two set the words
        // in slightly different sizes, and the editor's quoted original has
        // no place in the card.
        let out = 1.0 - smoothstep(0.28, 0.42, t);
        let inn = smoothstep(0.24, 0.4, t);
        // Inside it the editor's picture gives way to the card's, both
        // placed so their first lines of text meet, and scaled about that
        // line from the editor's size of type to the card's, so the letters
        // stay on each other too; each is at its own size at its own end of
        // the flight. Each is cut to its own card's shape: the pieces carry
        // a sliver of what was round their corners, which inside the moving
        // frame would show.
        let (ex, ey) = scene.editor_text;
        let (cx, cy) = scene.card_text;
        let (tx, ty) = (frame.x() + lerp(ex, cx), frame.y() + lerp(ey, cy));
        let size = lerp(1.0, scene.text_ratio);
        if let (true, Some(c)) = (out > 0.0, imp.editor_cut.borrow().as_ref()) {
            s.push_opacity(out);
            s.save();
            s.translate(&graphene::Point::new(tx, ty));
            s.scale(size, size);
            let at = graphene::Rect::new(-ex, -ey, editor.width(), editor.height());
            s.push_rounded_clip(&rounded_rect(&at, EDITOR_RADIUS, EDITOR_RADIUS));
            draw_piece(s, c, at.x() - editor.x(), at.y() - editor.y());
            s.pop();
            s.restore();
            s.pop();
        }
        if let (true, Some(c)) = (inn > 0.0, scene.card_cut.as_ref()) {
            let k = size / scene.text_ratio;
            s.push_opacity(inn);
            s.save();
            s.translate(&graphene::Point::new(tx, ty));
            s.scale(k, k);
            let at = graphene::Rect::new(-cx, -cy, card.width(), card.height());
            s.push_rounded_clip(&rounded_rect(&at, rt, rb));
            draw_piece(s, c, at.x() - card.x(), at.y() - card.y());
            s.pop();
            s.restore();
            s.pop();
        }
        s.pop();
    }
}


/// HYLKI_MORPH_SLOW=<factor> plays the morph that many times slower, to
/// look at it (development only).
fn slow_motion() -> f64 {
    thread_local! {
        static SLOW: f64 = std::env::var("HYLKI_MORPH_SLOW").ok().and_then(|v| v.parse().ok()).filter(|v: &f64| *v >= 1.0).unwrap_or(1.0);
    }
    SLOW.with(|s| *s)
}

/// HYLKI_MORPH_FRAMES=<dir> saves every frame of the morph there as a PNG
/// named by its time and progress (development only).
fn record_frame(obj: &SendMorph) {
    let Some(dir) = std::env::var_os("HYLKI_MORPH_FRAMES") else { return };
    if obj.width() <= 0 || obj.height() <= 0 {
        return;
    }
    let imp = obj.imp();
    let t = obj.motion_progress();
    let since = obj.elapsed_ms(imp.chrome_start.get());
    // The frame about to be drawn, drawn here first.
    let snap = gtk::Snapshot::new();
    let scale = imp.scale.get();
    snap.scale(scale, scale);
    obj.draw(&snap);
    let (Some(node), Some(renderer)) = (snap.to_node(), obj.native().and_then(|n| n.renderer())) else { return };
    let rect = graphene::Rect::new(0.0, 0.0, obj.width() as f32 * scale, obj.height() as f32 * scale);
    let tex = renderer.render_texture(&node, Some(&rect));
    let landed = if imp.scene.borrow().is_some() { "m" } else { "w" };
    let path = std::path::Path::new(&dir).join(format!("{since:07.1}-{landed}-{t:.3}.png"));
    let _ = tex.save_to_png(path);
}

/// With HYLKI_MORPH_FRAMES set, the pane itself once the stage is gone, to
/// hold the last frame against: the whole window, with the pane's box
/// beside it to crop by, since the pane alone leaves out the window's
/// ground (development only).
fn record_final(obj: &SendMorph) {
    let (Some(dir), Some(pane)) = (std::env::var_os("HYLKI_MORPH_FRAMES"), obj.parent()) else { return };
    for (ms, name) in [(30, "zz-final-a.png"), (400, "zz-final-b.png")] {
        let (dir, pane) = (dir.clone(), pane.clone());
        glib::timeout_add_local_once(std::time::Duration::from_millis(ms), move || {
            let Some(window) = pane.root().map(|r| r.upcast::<gtk::Widget>()) else { return };
            let Some(at) = pane.compute_bounds(&window) else { return };
            capture_next_frame(&window, move |still| {
                let Some((tex, scale)) = still else { return };
                let dir = std::path::Path::new(&dir);
                let _ = tex.save_to_png(dir.join(name));
                let crop = format!(
                    "{}x{}+{}+{}",
                    (at.width() * scale) as i32,
                    (at.height() * scale) as i32,
                    (at.x() * scale) as i32,
                    (at.y() * scale) as i32
                );
                let _ = std::fs::write(dir.join(format!("{name}.crop")), crop);
            });
        });
    }
}

/// A still of `widget` as it is drawn now, at the display's scale, and that
/// scale. Nothing when part of it waits to be redrawn: see
/// [`capture_next_frame`].
fn capture(widget: &gtk::Widget) -> Option<(gdk::Texture, f32)> {
    let (w, h) = (widget.width(), widget.height());
    if w <= 0 || h <= 0 {
        return None;
    }
    let native = widget.native()?;
    let renderer = native.renderer()?;
    let scale = native.surface().map(|s| s.scale()).unwrap_or(1.0) as f32;
    let paintable = gtk::WidgetPaintable::new(Some(widget));
    let snap = gtk::Snapshot::new();
    snap.scale(scale, scale);
    paintable.snapshot(&snap, w as f64, h as f64);
    let node = snap.to_node()?;
    let viewport = graphene::Rect::new(0.0, 0.0, (w as f32 * scale).round(), (h as f32 * scale).round());
    Some((renderer.render_texture(&node, Some(&viewport)), scale))
}

/// The color of one pixel of a still, at logical point (x, y).
fn sample(renderer: &gsk::Renderer, tex: &gdk::Texture, scale: f32, x: f32, y: f32) -> Option<gdk::RGBA> {
    let snap = gtk::Snapshot::new();
    snap.append_texture(tex, &graphene::Rect::new(0.0, 0.0, tex.width() as f32, tex.height() as f32));
    let node = snap.to_node()?;
    let (px, py) = ((x * scale).floor(), (y * scale).floor());
    if px < 0.0 || py < 0.0 || px >= tex.width() as f32 || py >= tex.height() as f32 {
        return None;
    }
    pixel(renderer, &node, px, py)
}

/// The window's own color behind the pane: at the first of `points` where
/// the pane's still is see-through, what the whole window draws there.
fn ground_behind(root: &gtk::Widget, still: &gdk::Texture, scale: f32, points: &[(f32, f32)]) -> Option<gdk::RGBA> {
    let renderer = root.native()?.renderer()?;
    let window: gtk::Widget = root.root()?.upcast();
    let &(x, y) = points
        .iter()
        .find(|(x, y)| sample(&renderer, still, scale, *x, *y).is_some_and(|c| c.alpha() < 0.02))?;
    let at = root.compute_point(&window, &graphene::Point::new(x, y))?;
    let snap = gtk::Snapshot::new();
    snap.scale(scale, scale);
    gtk::WidgetPaintable::new(Some(&window)).snapshot(&snap, window.width() as f64, window.height() as f64);
    let node = snap.to_node()?;
    pixel(&renderer, &node, (at.x() * scale).floor(), (at.y() * scale).floor())
}

/// One device pixel of a render node, at (px, py).
fn pixel(renderer: &gsk::Renderer, node: &gsk::RenderNode, px: f32, py: f32) -> Option<gdk::RGBA> {
    let one = renderer.render_texture(node, Some(&graphene::Rect::new(px, py, 1.0, 1.0)));
    let mut d = gdk::TextureDownloader::new(&one);
    d.set_format(gdk::MemoryFormat::R8g8b8a8);
    let (bytes, _) = d.download_bytes();
    let b = bytes.get(..4)?;
    Some(gdk::RGBA::new(b[0] as f32 / 255.0, b[1] as f32 / 255.0, b[2] as f32 / 255.0, b[3] as f32 / 255.0))
}

/// Draw the whole still, shifted by (dx, dy); callers clip to the part they
/// want. Shifts are snapped to device pixels so text stays sharp in motion.
fn draw_still(s: &gtk::Snapshot, tex: &gdk::Texture, scale: f32, full: &graphene::Rect, dx: f32, dy: f32) {
    let snap = |v: f32| (v * scale).round() / scale;
    let r = graphene::Rect::new(full.x() + snap(dx), full.y() + snap(dy), tex.width() as f32 / scale, tex.height() as f32 / scale);
    s.append_texture(tex, &r);
}

/// The part of `still` inside `rect`, less `hole`, as a texture of its own;
/// `rect` is widened to whole device pixels so the piece lands on them.
fn cut(
    renderer: &gsk::Renderer,
    still: &gdk::Texture,
    scale: f32,
    rect: &graphene::Rect,
    hole: Option<&graphene::Rect>,
) -> Option<Cut> {
    let x0 = (rect.x() * scale).floor();
    let y0 = (rect.y() * scale).floor();
    let x1 = ((rect.x() + rect.width()) * scale).ceil().min(still.width() as f32);
    let y1 = ((rect.y() + rect.height()) * scale).ceil().min(still.height() as f32);
    if x1 - x0 < 1.0 || y1 - y0 < 1.0 {
        return None;
    }
    let at = graphene::Rect::new(x0 / scale, y0 / scale, (x1 - x0) / scale, (y1 - y0) / scale);
    let full = graphene::Rect::new(0.0, 0.0, still.width() as f32 / scale, still.height() as f32 / scale);
    let snap = gtk::Snapshot::new();
    snap.scale(scale, scale);
    match hole {
        None => snap.append_texture(still, &full),
        Some(hole) => {
            for r in around(&at, hole) {
                snap.push_clip(&r);
                snap.append_texture(still, &full);
                snap.pop();
            }
        }
    }
    let node = snap.to_node()?;
    let tex = renderer.render_texture(&node, Some(&graphene::Rect::new(x0, y0, x1 - x0, y1 - y0)));
    Some(Cut { tex, at })
}

/// Draw a piece where it was cut from, shifted by (dx, dy), as it falls:
/// under a scale, device pixels are not where whole numbers put them.
fn draw_piece(s: &gtk::Snapshot, cut: &Cut, dx: f32, dy: f32) {
    let r = graphene::Rect::new(cut.at.x() + dx, cut.at.y() + dy, cut.at.width(), cut.at.height());
    s.append_scaled_texture(&cut.tex, gsk::ScalingFilter::Linear, &r);
}

/// Draw a piece where it was cut from, shifted by (dx, dy), on whole device
/// pixels.
fn draw_cut(s: &gtk::Snapshot, cut: &Cut, scale: f32, dx: f32, dy: f32) {
    let snap = |v: f32| (v * scale).round() / scale;
    let r = graphene::Rect::new(cut.at.x() + snap(dx), cut.at.y() + snap(dy), cut.at.width(), cut.at.height());
    s.append_texture(&cut.tex, &r);
}

fn rounded_rect(r: &graphene::Rect, top: f32, bottom: f32) -> gsk::RoundedRect {
    gsk::RoundedRect::new(
        *r,
        graphene::Size::new(top, top),
        graphene::Size::new(top, top),
        graphene::Size::new(bottom, bottom),
        graphene::Size::new(bottom, bottom),
    )
}

/// The lifted card's shadow, `lift` of the way up.
fn shadow(s: &gtk::Snapshot, card: &gsk::RoundedRect, lift: f32) {
    if lift <= 0.0 {
        return;
    }
    let shade = gdk::RGBA::new(0.0, 0.0, 0.0, 0.16 * lift);
    s.append_outset_shadow(card, &shade, 0.0, 6.0 * lift, 0.0, 22.0 * lift);
}

/// The parts of `outer` not covered by `hole`, as up to four rectangles.
fn around(outer: &graphene::Rect, hole: &graphene::Rect) -> Vec<graphene::Rect> {
    let (ox0, oy0) = (outer.x(), outer.y());
    let (ox1, oy1) = (ox0 + outer.width(), oy0 + outer.height());
    let hx0 = hole.x().clamp(ox0, ox1);
    let hy0 = hole.y().clamp(oy0, oy1);
    let hx1 = (hole.x() + hole.width()).clamp(ox0, ox1);
    let hy1 = (hole.y() + hole.height()).clamp(oy0, oy1);
    let mut v = Vec::new();
    let mut push = |x0: f32, y0: f32, x1: f32, y1: f32| {
        if x1 > x0 && y1 > y0 {
            v.push(graphene::Rect::new(x0, y0, x1 - x0, y1 - y0));
        }
    };
    push(ox0, oy0, ox1, hy0);
    push(ox0, hy1, ox1, oy1);
    push(ox0, hy0, hx0, hy1);
    push(hx1, hy0, ox1, hy1);
    v
}

fn inflate(r: &graphene::Rect, by: f32) -> graphene::Rect {
    graphene::Rect::new(r.x() - by, r.y() - by, r.width() + 2.0 * by, r.height() + 2.0 * by)
}

fn lerp_rect(a: &graphene::Rect, b: &graphene::Rect, e: f32) -> graphene::Rect {
    let l = |x: f32, y: f32| x + (y - x) * e;
    graphene::Rect::new(l(a.x(), b.x()), l(a.y(), b.y()), l(a.width(), b.width()), l(a.height(), b.height()))
}

fn mix(a: &gdk::RGBA, b: &gdk::RGBA, e: f32) -> gdk::RGBA {
    let l = |x: f32, y: f32| x + (y - x) * e;
    gdk::RGBA::new(l(a.red(), b.red()), l(a.green(), b.green()), l(a.blue(), b.blue()), l(a.alpha(), b.alpha()))
}

fn smoothstep(a: f64, b: f64, t: f64) -> f64 {
    let x = ((t - a) / (b - a)).clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

fn ease_out(t: f64) -> f64 {
    1.0 - (1.0 - t).powi(3)
}

/// The motion's curve: a cubic Bézier that leaves gently and arrives
/// softly, so the card neither jumps off the composer nor thuds into place.
fn ease(t: f64) -> f64 {
    cubic_bezier(0.32, 0.0, 0.12, 1.0, t)
}

fn cubic_bezier(x1: f64, y1: f64, x2: f64, y2: f64, t: f64) -> f64 {
    if t <= 0.0 {
        return 0.0;
    }
    if t >= 1.0 {
        return 1.0;
    }
    let bez = |a: f64, b: f64, u: f64| {
        let v = 1.0 - u;
        3.0 * v * v * u * a + 3.0 * v * u * u * b + u * u * u
    };
    // Solve x(u) = t by bisection: monotonic for x1, x2 in [0, 1].
    let (mut lo, mut hi) = (0.0, 1.0);
    for _ in 0..40 {
        let mid = (lo + hi) / 2.0;
        if bez(x1, x2, mid) < t {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    bez(y1, y2, (lo + hi) / 2.0)
}

/// What the send morph is made from: the pane (`root`, which the stage
/// covers), the composer's slot in it and the editor card inside that, and
/// the reader beside it with its web view.
pub struct Pieces {
    pub root: gtk::Widget,
    pub composer: gtk::Widget,
    pub editor: gtk::Widget,
    pub reader: gtk::Widget,
    pub view: webkit6::WebView,
    /// The reader's header bar, and whether it is showing now.
    pub header: Option<gtk::Widget>,
    pub header_shown: bool,
    /// The editor's web view, whose first line of text the card's meets.
    pub editor_view: Option<webkit6::WebView>,
}

/// Take the still the morph starts from and measure where the conversation's
/// cards stand, then answer `ready`: true once the pane may change under the
/// stage, false when the pane could not be captured.
pub fn start(stage: &SendMorph, p: Pieces, ready: impl FnOnce(bool) + 'static) {
    let stage = stage.clone();
    capture_next_frame(&p.root.clone(), move |still| {
        let bounds = |w: &gtk::Widget| w.compute_bounds(&p.root);
        let rects = (bounds(&p.composer), bounds(&p.editor), bounds(&p.reader), bounds(p.view.upcast_ref()));
        let (Some((still, scale)), (Some(composer), Some(editor), Some(reader), Some(view))) = (still, rects) else {
            tracing::info!(target: "hylki::morph", "send morph: the pane could not be captured");
            ready(false);
            return;
        };
        if editor.height() < 8.0 || view.height() < 8.0 {
            tracing::info!(target: "hylki::morph", "send morph: the editor or the reader has no height");
            ready(false);
            return;
        }
        let header = p.header.as_ref().filter(|_| p.header_shown).and_then(|h| bounds(h));
        // Where the reader leaves its background to the window, the window's
        // color is what shows.
        let ground = ground_behind(
            &p.root,
            &still,
            scale,
            &[
                (reader.x() + 2.0, reader.y() + 2.0),
                (reader.x() + 2.0, view.y() - 2.0),
                (reader.x() + reader.width() - 2.0, reader.y() + reader.height() - 2.0),
            ],
        );
        let before = Before { still, composer, editor, reader, view, header, ground, cards: Vec::new(), text: None };
        stage.begin(before, scale);
        // Two questions for the web views; the pane may change once both
        // are answered.
        let ready = std::rc::Rc::new(RefCell::new(Some(ready)));
        let left = std::rc::Rc::new(Cell::new(2u8));
        let answered = move || {
            left.set(left.get() - 1);
            if left.get() == 0 {
                if let Some(ready) = ready.borrow_mut().take() {
                    ready(true);
                }
            }
        };
        let answered = std::rc::Rc::new(answered);
        {
            let (stage, answered) = (stage.clone(), answered.clone());
            webkit6::prelude::WebViewExt::evaluate_javascript(
                &p.view,
                CARDS_JS,
                None,
                None,
                None::<&gtk::gio::Cancellable>,
                move |r| {
                    let cards = r.ok().and_then(|v| Measured::parse(&v.to_str(), &view)).map(|m| m.cards()).unwrap_or_default();
                    stage.set_before_cards(cards);
                    answered();
                },
            );
        }
        match p.editor_view.as_ref().and_then(|ev| Some((ev, ev.compute_bounds(&p.root)?))) {
            Some((ev, at)) => {
                webkit6::prelude::WebViewExt::evaluate_javascript(
                    ev,
                    EDITOR_TEXT_JS,
                    None,
                    None,
                    None::<&gtk::gio::Cancellable>,
                    move |r| {
                        let text = r.ok().and_then(|v| parse_text(&v.to_str(), &at));
                        if let Some(b) = stage.imp().before.borrow_mut().as_mut() {
                            b.text = text;
                        }
                        answered();
                    },
                );
            }
            None => answered(),
        }
    });
}

/// A still of `widget` taken just after the next frame is drawn. GTK keeps
/// each widget's last drawing and hands it to [`gtk::WidgetPaintable`], but
/// anything waiting to be redrawn (a blinking caret is enough) has none until
/// the frame is drawn; right after it, every part of the pane has one.
pub fn capture_next_frame(widget: &gtk::Widget, then: impl FnOnce(Option<(gdk::Texture, f32)>) + 'static) {
    type Then = Box<dyn FnOnce(Option<(gdk::Texture, f32)>)>;
    let then: std::rc::Rc<RefCell<Option<Then>>> = std::rc::Rc::new(RefCell::new(Some(Box::new(then))));
    let Some(clock) = widget.frame_clock() else {
        if let Some(f) = then.take() {
            f(capture(widget));
        }
        return;
    };
    let handler = std::rc::Rc::new(Cell::new(None));
    let tries = Cell::new(0);
    let id = {
        let (w, then, handler) = (widget.clone(), then.clone(), handler.clone());
        clock.connect_after_paint(move |clock| {
            let still = capture(&w);
            tries.set(tries.get() + 1);
            if still.is_none() && tries.get() < 3 {
                w.queue_draw();
                return;
            }
            if let Some(id) = handler.take() {
                clock.disconnect(id);
            }
            if let Some(f) = then.take() {
                f(still);
            }
        })
    };
    handler.set(Some(id));
    widget.queue_draw();
    // A window that stops drawing stops its clock: no still, then.
    glib::timeout_add_local_once(std::time::Duration::from_millis(250), move || {
        if let Some(id) = handler.take() {
            clock.disconnect(id);
        }
        if let Some(f) = then.take() {
            f(None);
        }
    });
}

/// Wait for the reply's card to be laid out in the reader, then land the
/// stage on it. The document is asked where its cards are every frame or
/// so; the card counts as settled once it is there, every message frame is
/// sized, and two answers in a row agree. A wait past
/// [`READY_TIMEOUT_MS`] fades the stage instead.
pub fn await_arrival(
    stage: &SendMorph,
    root: &gtk::Widget,
    reader: &gtk::Widget,
    view: &webkit6::WebView,
    header: Option<gtk::Widget>,
    key: String,
) {
    let poll = std::rc::Rc::new(Poll {
        stage: stage.clone(),
        header,
        root: root.clone(),
        reader: reader.clone(),
        view: view.clone(),
        key,
        started: std::time::Instant::now(),
        last: RefCell::new(None),
        same: Cell::new(0),
    });
    poll_once(poll);
}

struct Poll {
    stage: SendMorph,
    header: Option<gtk::Widget>,
    root: gtk::Widget,
    reader: gtk::Widget,
    view: webkit6::WebView,
    key: String,
    started: std::time::Instant,
    last: RefCell<Option<Measured>>,
    same: Cell<u32>,
}

fn poll_once(poll: std::rc::Rc<Poll>) {
    use webkit6::prelude::WebViewExt;
    if !poll.stage.running() {
        return;
    }
    if poll.started.elapsed() > std::time::Duration::from_millis(READY_TIMEOUT_MS) {
        tracing::info!(target: "hylki::morph", "send morph: the reply's card never settled ({:?})", poll.last.borrow().as_ref().map(|m| (m.live, m.cards.len())));
        poll.stage.abort();
        return;
    }
    let p = poll.clone();
    poll.view.evaluate_javascript(CARDS_JS, None, None, None::<&gtk::gio::Cancellable>, move |r| {
        let view_rect = p.view.compute_bounds(&p.root);
        let measured = r.ok().zip(view_rect).and_then(|(v, rect)| Measured::parse(&v.to_str(), &rect));
        let ready = measured.as_ref().is_some_and(|m| m.live && m.cards.iter().any(|(k, _, _)| *k == p.key));
        let same = ready && *p.last.borrow() == measured;
        p.same.set(if same { p.same.get() + 1 } else { 0 });
        p.last.replace(measured);
        // A document still moving after a while (pictures coming in, say)
        // is landed on as it stands: the card is there, and waiting on would
        // only keep the reply off the screen.
        let long = p.started.elapsed() > std::time::Duration::from_millis(SETTLE_MS);
        if p.same.get() >= 1 || (ready && long) {
            // Settled: the still is taken after the next frame, by when
            // GTK holds the web view's picture of it.
            land_on(&p);
            return;
        }
        let p2 = p.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(16), move || poll_once(p2));
    });
}

fn land_on(p: &Poll) {
    let (Some(m), Some(reader), Some(view)) =
        (p.last.borrow().clone(), p.reader.compute_bounds(&p.root), p.view.compute_bounds(&p.root))
    else {
        p.stage.abort();
        return;
    };
    let (stage, key, started) = (p.stage.clone(), p.key.clone(), p.started);
    let header = p.header.as_ref().and_then(|h| h.compute_bounds(&p.root)).filter(|r| r.height() > 0.0);
    capture_next_frame(&p.root, move |still| {
        let Some((still, _)) = still else {
            stage.abort();
            return;
        };
        tracing::info!(target: "hylki::morph", "send morph: landing after {:?}", started.elapsed());
        stage.land(After { still, reader, view, header, cards: m.cards(), arrival: key });
    });
}

/// The first widget under `root` (itself included) that `pick` accepts.
pub fn find_widget(root: &gtk::Widget, pick: &dyn Fn(&gtk::Widget) -> bool) -> Option<gtk::Widget> {
    if pick(root) {
        return Some(root.clone());
    }
    let mut child = root.first_child();
    while let Some(c) = child {
        if let Some(found) = find_widget(&c, pick) {
            return Some(found);
        }
        child = c.next_sibling();
    }
    None
}

/// The script that lists the reader document's cards: each one's key, its
/// box in the view's own pixels, and its header's height, with the view's
/// width in the document's pixels to scale them by.
pub const CARDS_JS: &str = concat!("(function(){", first_text_js!(), "var r=[];var ms=document.querySelectorAll('.hylki-msg');\
for(var i=0;i<ms.length;i++){var b=ms[i].getBoundingClientRect();\
var h=ms[i].querySelector('.hylki-msg-hdr');\
var f=ms[i].querySelector('iframe.hylki-frame'),t=null;\
try{var q=f&&f.contentDocument?ft(f.contentDocument):null;\
if(q){var fr=f.getBoundingClientRect();t=[fr.left+q[0],fr.top+q[1],q[2]];}}catch(_){}\
r.push([ms[i].dataset.key||'',b.left,b.top,b.width,b.height,h?h.getBoundingClientRect().height:0,t]);}\
var fs=document.querySelectorAll('iframe.hylki-frame'),live=true;\
for(var j=0;j<fs.length;j++){if(!fs[j].classList.contains('hylki-live'))live=false;}\
return JSON.stringify({w:window.innerWidth,y:window.scrollY,live:live,cards:r});})()");

/// The script that finds where the editor's first line of text begins.
pub const EDITOR_TEXT_JS: &str =
    concat!("(function(){", first_text_js!(), "return JSON.stringify({w:window.innerWidth,t:ft(document)});})()");

/// `ft(doc)`: the top-left corner of the first text a document shows, or
/// null. Text that is not laid out (styles, a folded quote) has no box.
macro_rules! first_text_js {
    () => {
        "function ft(d){var w=d.createTreeWalker(d.body,4),n;\
         while((n=w.nextNode())){if(!n.textContent.trim())continue;\
         var g=d.createRange();g.selectNodeContents(n);var c=g.getClientRects();\
         for(var i=0;i<c.length;i++){if(c[i].width>0){var z=parseFloat(d.documentElement.style.zoom)||1;\
         return [c[i].left,c[i].top,parseFloat(d.defaultView.getComputedStyle(n.parentElement).fontSize)*z];}}}\
         return null;}"
    };
}
use first_text_js;

/// Where [`EDITOR_TEXT_JS`] found the first line, in stage coordinates.
pub fn parse_text(json: &str, view: &graphene::Rect) -> Option<(graphene::Point, f32)> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let css_w = v.get("w")?.as_f64()? as f32;
    let k = if css_w > 0.0 { view.width() / css_w } else { 1.0 };
    let t = v.get("t")?.as_array()?;
    let n = |i: usize| t.get(i).and_then(|x| x.as_f64()).map(|x| x as f32);
    Some((graphene::Point::new(view.x() + n(0)? * k, view.y() + n(1)? * k), n(2).unwrap_or(0.0) * k))
}

/// What [`CARDS_JS`] reported, placed in stage coordinates given the web
/// view's box there.
#[derive(Clone, Debug, PartialEq)]
pub struct Measured {
    pub live: bool,
    /// The document's scroll, which must hold still too.
    pub scroll: f64,
    pub cards: Vec<(String, [f32; 5], Option<(f32, f32, f32)>)>,
}

impl Measured {
    pub fn parse(json: &str, view: &graphene::Rect) -> Option<Measured> {
        let v: serde_json::Value = serde_json::from_str(json).ok()?;
        let css_w = v.get("w")?.as_f64()? as f32;
        let k = if css_w > 0.0 { view.width() / css_w } else { 1.0 };
        let cards = v
            .get("cards")?
            .as_array()?
            .iter()
            .filter_map(|c| {
                let c = c.as_array()?;
                let key = c.first()?.as_str()?.to_string();
                let n = |i: usize| c.get(i).and_then(|x| x.as_f64()).map(|x| x as f32);
                let text = c.get(6).and_then(|t| t.as_array()).and_then(|t| {
                    let n = |i: usize| t.get(i).and_then(|x| x.as_f64()).map(|x| x as f32);
                    Some((view.x() + n(0)? * k, view.y() + n(1)? * k, n(2).unwrap_or(0.0) * k))
                });
                Some((
                    key,
                    [view.x() + n(1)? * k, view.y() + n(2)? * k, n(3)? * k, n(4)? * k, n(5)? * k],
                    text,
                ))
            })
            .collect();
        Some(Measured {
            live: v.get("live").and_then(|l| l.as_bool()).unwrap_or(false),
            scroll: v.get("y").and_then(|y| y.as_f64()).unwrap_or(0.0),
            cards,
        })
    }

    pub fn cards(&self) -> Vec<Card> {
        self.cards
            .iter()
            .map(|(key, b, t)| Card {
                key: key.clone(),
                rect: graphene::Rect::new(b[0], b[1], b[2], b[3]),
                head: b[4],
                text: t.map(|(x, y, size)| (graphene::Point::new(x, y), size)),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_curve_starts_and_ends_at_rest() {
        assert_eq!(ease(0.0), 0.0);
        assert_eq!(ease(1.0), 1.0);
        let mut last = 0.0;
        for i in 1..100 {
            let v = ease(i as f64 / 100.0);
            assert!(v >= last, "the curve never goes back");
            last = v;
        }
    }

    #[test]
    fn a_hole_leaves_four_strips() {
        let outer = graphene::Rect::new(0.0, 0.0, 100.0, 100.0);
        let hole = graphene::Rect::new(10.0, 20.0, 80.0, 50.0);
        let strips = around(&outer, &hole);
        let area: f32 = strips.iter().map(|r| r.width() * r.height()).sum();
        assert_eq!(area, 100.0 * 100.0 - 80.0 * 50.0);
    }

    #[test]
    fn measured_cards_land_in_stage_pixels() {
        let view = graphene::Rect::new(10.0, 100.0, 400.0, 300.0);
        let m = Measured::parse(r#"{"w":200,"y":0,"live":true,"cards":[["1:2",7,7,186,50,20]]}"#, &view).unwrap();
        let c = &m.cards()[0];
        assert_eq!(c.key, "1:2");
        assert_eq!((c.rect.x(), c.rect.y(), c.rect.width(), c.head), (24.0, 114.0, 372.0, 40.0));
    }
}
