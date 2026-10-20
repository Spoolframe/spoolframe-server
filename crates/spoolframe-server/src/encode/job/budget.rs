//! The encode budget (P3-10, PGPU-05): what each conversion weighs, the
//! places taken in it, and each GPU card's own count.

use std::collections::HashMap;
use std::sync::Arc;

use super::*;

/// A share of the encode budget (`transcode.max_jobs`), in quarters of
/// one software video encode: whole numbers, so admission never compares
/// floats.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Units(u32);

impl Units {
    /// What one software video encode weighs: `transcode.max_jobs = 1`.
    pub const PER_JOB: u32 = 4;

    /// `n` software video encodes' worth.
    pub const fn jobs(n: u32) -> Self {
        Self(n.saturating_mul(Self::PER_JOB))
    }

    /// `n` quarters of one.
    pub const fn quarters(n: u32) -> Self {
        Self(n)
    }

    /// In quarters.
    pub const fn get(self) -> u32 {
        self.0
    }

    /// In software video encodes: what the metrics report.
    pub fn as_jobs(self) -> f64 {
        f64::from(self.0) / f64::from(Self::PER_JOB)
    }
}

/// As `max_jobs` is written: `1.25`, `2`.
impl std::fmt::Display for Units {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        const { assert!(Units::PER_JOB == 4, "quarters, as the admin API shows them") };
        f.write_str(&spoolframe_admin_api::quarters(self.0))
    }
}

/// What a conversion asks of the machine, which says what it weighs
/// against the encode budget and which cap of its own it counts against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Load {
    /// Every stream copied into another container: disk and a file
    /// descriptor, next to no CPU. Bounded by `transcode.max_remux`.
    Remux,
    /// The video copied (or none), the audio encoded.
    Audio,
    /// The video encoded on a GPU (VA-API, NVENC), with its decode or a
    /// filter (tone mapping, burn-in) on the CPU. Bounded by
    /// `transcode.max_gpu_jobs` too.
    Gpu,
    /// The video encoded in software (libx264, libx265).
    Software,
    /// The video decoded, scaled and encoded on a GPU (VA-API), with no
    /// filter on the CPU. Bounded by `transcode.max_gpu_jobs` too.
    GpuOnly,
}

impl Load {
    pub const ALL: [Load; 5] = [
        Load::Remux,
        Load::Audio,
        Load::Gpu,
        Load::Software,
        Load::GpuOnly,
    ];

    /// What it weighs against `transcode.max_jobs`. Measured on a 32-core
    /// machine, for 60 s of output, a remux took under 1% of the CPU time
    /// of a libx264 encode of the same source and an audio-only encode
    /// about 0.5%. A VA-API encode took 10 to 45% decoding in software,
    /// 1.5 to 26% decoding on the GPU with its frames back in memory, and
    /// 0.4 to 1.7% all on the GPU; one tone-mapping 4K HDR on the CPU
    /// took 70%. An audio encode, and one all on the GPU, weigh more than
    /// they cost, as the least a conversion that encodes can weigh.
    pub fn weight(self) -> Units {
        match self {
            Load::Remux => Units::quarters(0),
            Load::Audio | Load::GpuOnly => Units::quarters(1),
            Load::Gpu => Units::quarters(2),
            Load::Software => Units::jobs(1),
        }
    }

    /// Its name in the metrics' `kind` label.
    pub fn as_str(self) -> &'static str {
        match self {
            Load::Remux => "remux",
            Load::Audio => "audio",
            Load::Gpu => "gpu",
            Load::Software => "software",
            Load::GpuOnly => "gpu_only",
        }
    }

    /// It decodes and encodes a stream again, rather than only copying.
    pub fn encodes(self) -> bool {
        self != Load::Remux
    }
}

/// A conversion's place in the budget. A job that replaces another (a
/// seek, a subtitle change) with the same load shares the place of the
/// one it replaces, so nobody else can take it between the two; it is
/// free once both have let go of it.
#[derive(Debug, Clone)]
pub struct Slot {
    pub(super) place: Arc<Place>,
}

impl Slot {
    pub fn load(&self) -> Load {
        self.place.load
    }

    /// The GPU card it counts against, for a GPU video encode.
    pub fn card(&self) -> Option<&CardLimit> {
        self.place.card.as_ref()
    }
}

/// A GPU card a GPU video encode counts against (PGPU-05): each card has
/// its own count, and its own limit or `transcode.max_gpu_jobs`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CardLimit {
    /// Which card: `gpus::Card::key`.
    pub key: String,
    /// Its entry's `max_jobs`; `None` for `transcode.max_gpu_jobs`.
    pub max_jobs: Option<usize>,
}

/// The budget's use now, and its limits.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BudgetUse {
    /// Taken by the conversions running now.
    pub used: Units,
    /// `transcode.max_jobs`.
    pub max: Units,
    /// Remuxes running, and `transcode.max_remux`.
    pub remuxes: usize,
    pub max_remux: usize,
    /// GPU video encodes running on every card together, and
    /// `transcode.max_gpu_jobs`: each card's limit unless its entry sets
    /// one (PGPU-05).
    pub gpu_jobs: usize,
    pub max_gpu_jobs: usize,
    /// GPU video encodes running on each card, by `CardLimit::key`. A GPU
    /// encode with no card (none was detected) counts under `""`.
    pub gpu_jobs_by_card: HashMap<String, usize>,
}

/// A card's key in [`BudgetUse::gpu_jobs_by_card`].
fn card_key(card: Option<&CardLimit>) -> &str {
    card.map_or("", |c| c.key.as_str())
}

impl BudgetUse {
    /// Whether a conversion of `load` on `card` may start now.
    /// `replacing`: the load and card of the conversion it replaces, which
    /// still runs but ends once this one has started, so what it holds
    /// counts as free.
    fn admits(
        &self,
        load: Load,
        card: Option<&CardLimit>,
        replacing: Option<(Load, Option<&CardLimit>)>,
    ) -> bool {
        let mut now = self.clone();
        if let Some((old, old_card)) = replacing {
            now.give_back(old, old_card);
        }
        let fits = now.used.0 + load.weight().0 <= now.max.0;
        match load {
            Load::Remux => now.remuxes < now.max_remux,
            Load::Gpu | Load::GpuOnly => fits && now.card_has_room(card),
            Load::Audio | Load::Software => fits,
        }
    }

    /// Whether `card` runs fewer GPU encodes than its limit.
    pub fn card_has_room(&self, card: Option<&CardLimit>) -> bool {
        let running = self
            .gpu_jobs_by_card
            .get(card_key(card))
            .copied()
            .unwrap_or(0);
        let max = card.and_then(|c| c.max_jobs).unwrap_or(self.max_gpu_jobs);
        running < max
    }

    fn take(&mut self, load: Load, card: Option<&CardLimit>) {
        self.used.0 += load.weight().0;
        match load {
            Load::Remux => self.remuxes += 1,
            Load::Gpu | Load::GpuOnly => {
                self.gpu_jobs += 1;
                *self
                    .gpu_jobs_by_card
                    .entry(card_key(card).to_owned())
                    .or_default() += 1;
            }
            Load::Audio | Load::Software => {}
        }
    }

    fn give_back(&mut self, load: Load, card: Option<&CardLimit>) {
        self.used.0 -= load.weight().0;
        match load {
            Load::Remux => self.remuxes -= 1,
            Load::Gpu | Load::GpuOnly => {
                self.gpu_jobs -= 1;
                let key = card_key(card);
                if let Some(running) = self.gpu_jobs_by_card.get_mut(key) {
                    *running -= 1;
                    if *running == 0 {
                        self.gpu_jobs_by_card.remove(key);
                    }
                }
            }
            Load::Audio | Load::Software => {}
        }
    }

    pub(super) fn set_limits(&mut self, limits: &JobLimits) {
        self.max = limits.budget;
        self.max_remux = limits.max_remux;
        self.max_gpu_jobs = limits.max_gpu_jobs;
    }
}

/// A taken place, given back when dropped. A counter rather than a
/// semaphore: the limits are live settings (P3-10), and a lower one takes
/// effect as jobs end, without stopping any.
#[derive(Debug)]
pub(super) struct Place {
    budget: Arc<parking_lot::Mutex<BudgetUse>>,
    load: Load,
    /// The card a GPU encode counts against.
    card: Option<CardLimit>,
    /// Told as the place is given back ([`Jobs::place_released`]).
    released: Arc<tokio::sync::Notify>,
}

impl Place {
    pub(super) fn take(
        budget: &Arc<parking_lot::Mutex<BudgetUse>>,
        released: &Arc<tokio::sync::Notify>,
        load: Load,
        card: Option<CardLimit>,
        replacing: Option<&Slot>,
    ) -> Option<Self> {
        // Only a GPU encode counts against a card.
        let card = card.filter(|_| matches!(load, Load::Gpu | Load::GpuOnly));
        let mut now = budget.lock();
        if !now.admits(load, card.as_ref(), replacing.map(|r| (r.load(), r.card()))) {
            return None;
        }
        now.take(load, card.as_ref());
        Some(Self {
            budget: budget.clone(),
            load,
            card,
            released: released.clone(),
        })
    }
}

impl Drop for Place {
    fn drop(&mut self) {
        self.budget.lock().give_back(self.load, self.card.as_ref());
        self.released.notify_one();
    }
}

/// A place held by work that gives it up to live playback (PX-37): the
/// copy pool's. The jobs keep a weak handle on each, newest last, for
/// live admission to take one from ([`Jobs::hold_yielding`]).
#[derive(Debug)]
pub(crate) struct YieldEntry {
    pub(super) slot: Slot,
    /// Set once live admission took its place.
    pub(super) preempted: tokio::sync::watch::Sender<bool>,
}

/// A yielding hold (PX-37): a place in the budget until it is dropped,
/// unless live playback that finds no other place takes it first; then
/// [`Yielding::yielded`] resolves, and the holder kills its ffmpeg and
/// starts over later. Dropping it gives the place back; the jobs' handle
/// on it is weak, so it is no longer one live admission can take.
#[derive(Debug)]
pub(crate) struct Yielding {
    pub(super) entry: Arc<YieldEntry>,
}

impl Yielding {
    /// Resolves once live admission has taken its place. The place is
    /// counted twice until this hold is dropped, as a seek's run and the
    /// run it replaces both are until the old one stops.
    pub(crate) async fn yielded(&self) {
        let mut preempted = self.entry.preempted.subscribe();
        // The sender is the entry's, which this hold keeps: never closed.
        let _ = preempted.wait_for(|taken| *taken).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A budget of `jobs` software video encodes, with the default caps.
    fn budget(jobs: u32) -> Arc<parking_lot::Mutex<BudgetUse>> {
        budget_with(jobs, 32, 3)
    }

    fn budget_with(
        jobs: u32,
        max_remux: usize,
        max_gpu_jobs: usize,
    ) -> Arc<parking_lot::Mutex<BudgetUse>> {
        Arc::new(parking_lot::Mutex::new(BudgetUse {
            max: Units::jobs(jobs),
            max_remux,
            max_gpu_jobs,
            ..BudgetUse::default()
        }))
    }

    fn take(budget: &Arc<parking_lot::Mutex<BudgetUse>>, load: Load) -> Option<Place> {
        Place::take(budget, &Arc::default(), load, None, None)
    }

    fn card(key: &str, max_jobs: Option<usize>) -> CardLimit {
        CardLimit {
            key: key.into(),
            max_jobs,
        }
    }

    fn take_on(
        budget: &Arc<parking_lot::Mutex<BudgetUse>>,
        load: Load,
        card: &CardLimit,
    ) -> Option<Place> {
        Place::take(budget, &Arc::default(), load, Some(card.clone()), None)
    }

    /// PGPU-05: `max_gpu_jobs` is each card's, not all of theirs: a full
    /// card leaves the other's room, and an entry's `max_jobs` is its
    /// cards' own limit.
    #[test]
    fn each_card_has_its_own_gpu_cap() {
        let budget = budget_with(8, 32, 2);
        let (xtx, igpu) = (
            card("vaapi:0000:03:00.0", None),
            card("vaapi:0000:6d:00.0", Some(1)),
        );
        let _a = take_on(&budget, Load::Gpu, &xtx).unwrap();
        let _b = take_on(&budget, Load::GpuOnly, &xtx).unwrap();
        assert!(
            take_on(&budget, Load::Gpu, &xtx).is_none(),
            "a third on a card of 2"
        );
        assert!(!budget.lock().card_has_room(Some(&xtx)));
        assert!(budget.lock().card_has_room(Some(&igpu)));
        let c = take_on(&budget, Load::Gpu, &igpu).unwrap();
        assert!(
            take_on(&budget, Load::Gpu, &igpu).is_none(),
            "its entry says 1"
        );
        assert_eq!(budget.lock().gpu_jobs, 3);
        drop(c);
        assert!(budget.lock().card_has_room(Some(&igpu)));
        assert_eq!(
            budget.lock().gpu_jobs_by_card.get("vaapi:0000:6d:00.0"),
            None
        );
        // A software encode counts against no card.
        let _software = take_on(&budget, Load::Software, &xtx).unwrap();
        assert_eq!(budget.lock().gpu_jobs_by_card["vaapi:0000:03:00.0"], 2);
    }

    /// A full card leaves a spilled software encode (`pick::spill`) the
    /// CPU's budget: it starts while `max_jobs` has room, and only then
    /// is a conversion refused as busy.
    #[test]
    fn a_spill_from_a_full_card_runs_on_the_cpu_budget() {
        let budget = budget_with(2, 32, 1);
        let xtx = card("vaapi:0000:03:00.0", None);
        let _gpu = take_on(&budget, Load::Gpu, &xtx).unwrap();
        assert!(
            take_on(&budget, Load::Gpu, &xtx).is_none(),
            "the card is full"
        );
        let _spilled = take(&budget, Load::Software).expect("the CPU has room");
        assert!(
            take(&budget, Load::Software).is_none(),
            "half a job and one more fill 2: busy"
        );
    }

    /// A job that replaces another counts the replaced one's place as
    /// free, on the card it was on.
    #[test]
    fn a_replaced_job_s_card_counts_as_free() {
        let budget = budget_with(8, 32, 1);
        let one = card("nvenc:0000:01:00.0", None);
        let old = Slot {
            place: Arc::new(take_on(&budget, Load::Gpu, &one).unwrap()),
        };
        assert!(take_on(&budget, Load::GpuOnly, &one).is_none());
        let new = Place::take(
            &budget,
            &Arc::default(),
            Load::GpuOnly,
            Some(one.clone()),
            Some(&old),
        );
        assert!(new.is_some(), "the old one's place on the card is free");
    }

    #[test]
    fn a_lower_limit_takes_effect_as_jobs_end_and_a_higher_one_at_once() {
        let budget = budget(2);
        let first = take(&budget, Load::Software).unwrap();
        let second = take(&budget, Load::Software).unwrap();
        assert!(take(&budget, Load::Software).is_none());
        budget.lock().max = Units::jobs(1);
        drop(first);
        // Still one taken, and one is all there is.
        assert!(take(&budget, Load::Software).is_none());
        drop(second);
        let third = take(&budget, Load::Software).unwrap();
        budget.lock().max = Units::jobs(3);
        let _fourth = take(&budget, Load::Software).unwrap();
        drop(third);
    }

    #[test]
    fn four_audio_encodes_fill_a_budget_of_one() {
        let budget = budget(1);
        let four: Vec<Place> = (0..4)
            .map(|n| take(&budget, Load::Audio).unwrap_or_else(|| panic!("audio encode {n}")))
            .collect();
        assert!(take(&budget, Load::Audio).is_none(), "a fifth fits");
        assert_eq!(budget.lock().used, Units::jobs(1));
        drop(four);
        assert_eq!(budget.lock().used, Units::default());
    }

    #[test]
    fn remuxes_weigh_nothing_and_run_up_to_their_own_cap() {
        let budget = budget_with(1, 5, 3);
        let _video = take(&budget, Load::Software).unwrap();
        let remuxes: Vec<Place> = (0..5)
            .map(|n| take(&budget, Load::Remux).unwrap_or_else(|| panic!("remux {n}")))
            .collect();
        assert!(take(&budget, Load::Remux).is_none(), "a sixth remux fits");
        let used = budget.lock().clone();
        assert_eq!((used.used, used.remuxes), (Units::jobs(1), 5));
        drop(remuxes);
        assert_eq!(budget.lock().remuxes, 0);
    }

    #[test]
    fn a_video_encode_needs_a_whole_unit_where_an_audio_one_needs_a_quarter() {
        let budget = budget(1);
        let _three: Vec<Place> = (0..3)
            .map(|_| take(&budget, Load::Audio).unwrap())
            .collect();
        assert_eq!(budget.lock().used, Units::quarters(3));
        assert!(take(&budget, Load::Software).is_none(), "1.75 of 1");
        assert!(take(&budget, Load::Gpu).is_none(), "1.25 of 1");
        assert!(take(&budget, Load::Audio).is_some());
    }

    #[test]
    fn gpu_encodes_weigh_half_and_stop_at_their_own_cap() {
        let budget = budget_with(4, 32, 2);
        let first = take(&budget, Load::Gpu).unwrap();
        let _second = take(&budget, Load::Gpu).unwrap();
        assert_eq!(budget.lock().used, Units::jobs(1));
        // 3 of the budget is free, but the GPU cap is reached.
        assert!(take(&budget, Load::Gpu).is_none());
        let _software = take(&budget, Load::Software).unwrap();
        drop(first);
        assert!(take(&budget, Load::Gpu).is_some());
    }

    /// Decoded, scaled and encoded on the GPU, a conversion takes 1 to 2%
    /// of a software encode's CPU: the least an encode weighs. It is still
    /// a GPU job.
    #[test]
    fn a_conversion_all_on_the_gpu_weighs_a_quarter_under_the_gpu_cap() {
        let budget = budget_with(1, 32, 3);
        let first = take(&budget, Load::GpuOnly).unwrap();
        let _second = take(&budget, Load::GpuOnly).unwrap();
        assert_eq!(budget.lock().used, Units::quarters(2));
        assert_eq!(budget.lock().gpu_jobs, 2);
        // The cap counts both kinds of GPU job.
        let _third = take(&budget, Load::Gpu).unwrap();
        assert_eq!(budget.lock().used, Units::jobs(1));
        drop(first);
        let budget = budget_with(4, 32, 2);
        let _gpu = take(&budget, Load::Gpu).unwrap();
        let _only = take(&budget, Load::GpuOnly).unwrap();
        assert!(take(&budget, Load::GpuOnly).is_none(), "over the GPU cap");
        assert_eq!(budget.lock().gpu_jobs, 2);
    }

    #[test]
    fn a_replacement_of_another_load_counts_what_it_replaces_as_free() {
        let budget = budget(1);
        let audio = Slot {
            place: Arc::new(take(&budget, Load::Audio).unwrap()),
        };
        let _others: Vec<Place> = (0..3)
            .map(|_| take(&budget, Load::Audio).unwrap())
            .collect();
        // A subtitle burned in where an audio encode ran: a software video
        // encode, which fits only once that audio encode's quarter is free.
        assert!(take(&budget, Load::Software).is_none());
        drop(_others);
        let video =
            Place::take(&budget, &Arc::default(), Load::Software, None, Some(&audio)).unwrap();
        // Both run for a moment, until the old one stops.
        assert_eq!(budget.lock().used, Units::quarters(5));
        drop(audio);
        assert_eq!(budget.lock().used, Units::jobs(1));
        drop(video);
    }

    #[test]
    fn units_read_as_max_jobs_is_written() {
        assert_eq!(Units::jobs(2).to_string(), "2");
        assert_eq!(Units::quarters(5).to_string(), "1.25");
        assert_eq!(Units::quarters(2).to_string(), "0.5");
        assert_eq!(Units::quarters(7).to_string(), "1.75");
        assert_eq!(Units::quarters(5).as_jobs(), 1.25);
    }

    #[tokio::test]
    async fn new_limits_apply_to_the_next_job_but_not_the_scratch_directory() {
        let tmp = spoolframe_test_dir::TestDir::new("spoolframe-job-limits").await;
        let limits = JobLimits {
            budget: Units::jobs(2),
            max_remux: 32,
            max_gpu_jobs: 3,
            scratch_dir: tmp.as_ref().join("scratch"),
            scratch_limit_bytes: 1 << 30,
            read_ahead: Duration::from_secs(60),
            copy_segment_secs: 2,
            stalled_after: STALLED_AFTER,
            gpu_filters: false,
            tone_map: spoolframe_media::encoders::ToneMapSetting::Auto,
            video_codecs: crate::config::DEFAULT_VIDEO_CODECS.to_vec(),
        };
        let jobs = Jobs::new(limits.clone(), Tool::unconfined("ffmpeg")).unwrap();
        jobs.set_limits(JobLimits {
            budget: Units::jobs(1),
            max_remux: 1,
            max_gpu_jobs: 1,
            scratch_dir: tmp.as_ref().join("elsewhere"),
            read_ahead: Duration::from_secs(5),
            copy_segment_secs: 2,
            ..limits.clone()
        });
        let now = jobs.limits();
        assert_eq!(now.budget, Units::jobs(1));
        assert_eq!(now.read_ahead, Duration::from_secs(5));
        assert_eq!(now.scratch_dir, limits.scratch_dir);
        let budget = jobs.budget();
        assert_eq!(
            (budget.max, budget.max_remux, budget.max_gpu_jobs),
            (Units::jobs(1), 1, 1)
        );
        let _taken = take(&jobs.budget, Load::Software).unwrap();
        assert!(take(&jobs.budget, Load::Software).is_none());
        let _remux = take(&jobs.budget, Load::Remux).unwrap();
        assert!(take(&jobs.budget, Load::Remux).is_none());
    }
}
