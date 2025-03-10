use crate::util::{
  get_activity_cached,
  get_actor_cached,
  get_latest_activity_id,
  LEMMY_TEST_FAST_FEDERATION,
  WORK_FINISHED_RECHECK_DELAY,
};
use activitypub_federation::{activity_sending::SendActivityTask, config::Data};
use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use lemmy_api_common::{context::LemmyContext, federate_retry_sleep_duration};
use lemmy_apub::activity_lists::SharedInboxActivities;
use lemmy_db_schema::{
  newtypes::{ActivityId, CommunityId, InstanceId},
  source::{
    activity::SentActivity,
    federation_queue_state::FederationQueueState,
    instance::Instance,
    site::Site,
  },
  utils::DbPool,
};
use lemmy_db_views_actor::structs::CommunityFollowerView;
use lemmy_utils::error::LemmyErrorExt2;
use once_cell::sync::Lazy;
use reqwest::Url;
use std::{
  collections::{HashMap, HashSet},
  time::Duration,
};
use tokio::{sync::mpsc::UnboundedSender, time::sleep};
use tokio_util::sync::CancellationToken;

/// Check whether to save state to db every n sends if there's no failures (during failures state is saved after every attempt)
/// This determines the batch size for loop_batch. After a batch ends and SAVE_STATE_EVERY_TIME has passed, the federation_queue_state is updated in the DB.
static CHECK_SAVE_STATE_EVERY_IT: i64 = 100;
/// Save state to db after this time has passed since the last state (so if the server crashes or is SIGKILLed, less than X seconds of activities are resent)
static SAVE_STATE_EVERY_TIME: Duration = Duration::from_secs(60);
/// interval with which new additions to community_followers are queried.
///
/// The first time some user on an instance follows a specific remote community (or, more precisely: the first time a (followed_community_id, follower_inbox_url) tuple appears),
/// this delay limits the maximum time until the follow actually results in activities from that community id being sent to that inbox url.
/// This delay currently needs to not be too small because the DB load is currently fairly high because of the current structure of storing inboxes for every person, not having a separate list of shared_inboxes, and the architecture of having every instance queue be fully separate.
/// (see https://github.com/LemmyNet/lemmy/issues/3958)
static FOLLOW_ADDITIONS_RECHECK_DELAY: Lazy<chrono::Duration> = Lazy::new(|| {
  if *LEMMY_TEST_FAST_FEDERATION {
    chrono::Duration::seconds(1)
  } else {
    chrono::Duration::minutes(2)
  }
});
/// The same as FOLLOW_ADDITIONS_RECHECK_DELAY, but triggering when the last person on an instance unfollows a specific remote community.
/// This is expected to happen pretty rarely and updating it in a timely manner is not too important.
static FOLLOW_REMOVALS_RECHECK_DELAY: Lazy<chrono::Duration> =
  Lazy::new(|| chrono::Duration::hours(1));
pub(crate) struct InstanceWorker {
  instance: Instance,
  // load site lazily because if an instance is first seen due to being on allowlist,
  // the corresponding row in `site` may not exist yet since that is only added once
  // `fetch_instance_actor_for_object` is called.
  // (this should be unlikely to be relevant outside of the federation tests)
  site_loaded: bool,
  site: Option<Site>,
  followed_communities: HashMap<CommunityId, HashSet<Url>>,
  stop: CancellationToken,
  context: Data<LemmyContext>,
  stats_sender: UnboundedSender<(String, FederationQueueState)>,
  last_full_communities_fetch: DateTime<Utc>,
  last_incremental_communities_fetch: DateTime<Utc>,
  state: FederationQueueState,
  last_state_insert: DateTime<Utc>,
}

impl InstanceWorker {
  pub(crate) async fn init_and_loop(
    instance: Instance,
    context: Data<LemmyContext>,
    pool: &mut DbPool<'_>, // in theory there's a ref to the pool in context, but i couldn't get that to work wrt lifetimes
    stop: CancellationToken,
    stats_sender: UnboundedSender<(String, FederationQueueState)>,
  ) -> Result<(), anyhow::Error> {
    let state = FederationQueueState::load(pool, instance.id).await?;
    let mut worker = InstanceWorker {
      instance,
      site_loaded: false,
      site: None,
      followed_communities: HashMap::new(),
      stop,
      context,
      stats_sender,
      last_full_communities_fetch: Utc.timestamp_nanos(0),
      last_incremental_communities_fetch: Utc.timestamp_nanos(0),
      state,
      last_state_insert: Utc.timestamp_nanos(0),
    };
    worker.loop_until_stopped(pool).await
  }
  /// loop fetch new activities from db and send them to the inboxes of the given instances
  /// this worker only returns if (a) there is an internal error or (b) the cancellation token is cancelled (graceful exit)
  pub(crate) async fn loop_until_stopped(
    &mut self,
    pool: &mut DbPool<'_>,
  ) -> Result<(), anyhow::Error> {
    let save_state_every = chrono::Duration::from_std(SAVE_STATE_EVERY_TIME).expect("not negative");

    self.update_communities(pool).await?;
    self.initial_fail_sleep().await?;
    while !self.stop.is_cancelled() {
      self.loop_batch(pool).await?;
      if self.stop.is_cancelled() {
        break;
      }
      if (Utc::now() - self.last_state_insert) > save_state_every {
        self.save_and_send_state(pool).await?;
      }
      self.update_communities(pool).await?;
    }
    // final update of state in db
    self.save_and_send_state(pool).await?;
    Ok(())
  }

  async fn initial_fail_sleep(&mut self) -> Result<()> {
    // before starting queue, sleep remaining duration if last request failed
    if self.state.fail_count > 0 {
      let last_retry = self
        .state
        .last_retry
        .context("impossible: if fail count set last retry also set")?;
      let elapsed = (Utc::now() - last_retry).to_std()?;
      let required = federate_retry_sleep_duration(self.state.fail_count);
      if elapsed >= required {
        return Ok(());
      }
      let remaining = required - elapsed;
      tokio::select! {
        () = sleep(remaining) => {},
        () = self.stop.cancelled() => {}
      }
    }
    Ok(())
  }
  /// send out a batch of CHECK_SAVE_STATE_EVERY_IT activities
  async fn loop_batch(&mut self, pool: &mut DbPool<'_>) -> Result<()> {
    let latest_id = get_latest_activity_id(pool).await?;
    let mut id = if let Some(id) = self.state.last_successful_id {
      id
    } else {
      // this is the initial creation (instance first seen) of the federation queue for this instance
      // skip all past activities:
      self.state.last_successful_id = Some(latest_id);
      // save here to ensure it's not read as 0 again later if no activities have happened
      self.save_and_send_state(pool).await?;
      latest_id
    };
    if id == latest_id {
      // no more work to be done, wait before rechecking
      tokio::select! {
        () = sleep(*WORK_FINISHED_RECHECK_DELAY) => {},
        () = self.stop.cancelled() => {}
      }
      return Ok(());
    }
    let mut processed_activities = 0;
    while id < latest_id
      && processed_activities < CHECK_SAVE_STATE_EVERY_IT
      && !self.stop.is_cancelled()
    {
      id = ActivityId(id.0 + 1);
      processed_activities += 1;
      let Some(ele) = get_activity_cached(pool, id)
        .await
        .context("failed reading activity from db")?
      else {
        self.state.last_successful_id = Some(id);
        continue;
      };
      if let Err(e) = self.send_retry_loop(pool, &ele.0, &ele.1).await {
        tracing::warn!(
          "sending {} errored internally, skipping activity: {:?}",
          ele.0.ap_id,
          e
        );
      }
      if self.stop.is_cancelled() {
        return Ok(());
      }
      // send success!
      self.state.last_successful_id = Some(id);
      self.state.last_successful_published_time = Some(ele.0.published);
      self.state.fail_count = 0;
    }
    Ok(())
  }

  // this function will return successfully when (a) send succeeded or (b) worker cancelled
  // and will return an error if an internal error occurred (send errors cause an infinite loop)
  async fn send_retry_loop(
    &mut self,
    pool: &mut DbPool<'_>,
    activity: &SentActivity,
    object: &SharedInboxActivities,
  ) -> Result<()> {
    let inbox_urls = self
      .get_inbox_urls(pool, activity)
      .await
      .context("failed figuring out inbox urls")?;
    if inbox_urls.is_empty() {
      self.state.last_successful_id = Some(activity.id);
      self.state.last_successful_published_time = Some(activity.published);
      return Ok(());
    }
    let Some(actor_apub_id) = &activity.actor_apub_id else {
      return Ok(()); // activity was inserted before persistent queue was activated
    };
    let actor = get_actor_cached(pool, activity.actor_type, actor_apub_id)
      .await
      .context("failed getting actor instance (was it marked deleted / removed?)")?;

    let inbox_urls = inbox_urls.into_iter().collect();
    let requests = SendActivityTask::prepare(object, actor.as_ref(), inbox_urls, &self.context)
      .await
      .into_anyhow()?;
    for task in requests {
      // usually only one due to shared inbox
      tracing::info!("sending out {}", task);
      while let Err(e) = task.sign_and_send(&self.context).await {
        self.state.fail_count += 1;
        self.state.last_retry = Some(Utc::now());
        let retry_delay: Duration = federate_retry_sleep_duration(self.state.fail_count);
        tracing::info!(
          "{}: retrying {:?} attempt {} with delay {retry_delay:.2?}. ({e})",
          self.instance.domain,
          activity.id,
          self.state.fail_count
        );
        self.save_and_send_state(pool).await?;
        tokio::select! {
          () = sleep(retry_delay) => {},
          () = self.stop.cancelled() => {
            // save state to db and exit
            return Ok(());
          }
        }
      }
    }
    Ok(())
  }

  /// get inbox urls of sending the given activity to the given instance
  /// most often this will return 0 values (if instance doesn't care about the activity)
  /// or 1 value (the shared inbox)
  /// > 1 values only happens for non-lemmy software
  async fn get_inbox_urls(
    &mut self,
    pool: &mut DbPool<'_>,
    activity: &SentActivity,
  ) -> Result<HashSet<Url>> {
    let mut inbox_urls: HashSet<Url> = HashSet::new();

    if activity.send_all_instances {
      if !self.site_loaded {
        self.site = Site::read_from_instance_id(pool, self.instance.id).await?;
        self.site_loaded = true;
      }
      if let Some(site) = &self.site {
        // Nutomic: Most non-lemmy software wont have a site row. That means it cant handle these activities. So handling it like this is fine.
        inbox_urls.insert(site.inbox_url.inner().clone());
      }
    }
    if let Some(t) = &activity.send_community_followers_of {
      if let Some(urls) = self.followed_communities.get(t) {
        inbox_urls.extend(urls.iter().map(std::clone::Clone::clone));
      }
    }
    inbox_urls.extend(
      activity
        .send_inboxes
        .iter()
        .filter_map(std::option::Option::as_ref)
        .filter(|&u| (u.domain() == Some(&self.instance.domain)))
        .map(|u| u.inner().clone()),
    );
    Ok(inbox_urls)
  }

  async fn update_communities(&mut self, pool: &mut DbPool<'_>) -> Result<()> {
    if (Utc::now() - self.last_full_communities_fetch) > *FOLLOW_REMOVALS_RECHECK_DELAY {
      // process removals every hour
      (self.followed_communities, self.last_full_communities_fetch) = self
        .get_communities(pool, self.instance.id, Utc.timestamp_nanos(0))
        .await?;
      self.last_incremental_communities_fetch = self.last_full_communities_fetch;
    }
    if (Utc::now() - self.last_incremental_communities_fetch) > *FOLLOW_ADDITIONS_RECHECK_DELAY {
      // process additions every minute
      let (news, time) = self
        .get_communities(
          pool,
          self.instance.id,
          self.last_incremental_communities_fetch,
        )
        .await?;
      self.followed_communities.extend(news);
      self.last_incremental_communities_fetch = time;
    }
    Ok(())
  }

  /// get a list of local communities with the remote inboxes on the given instance that cares about them
  async fn get_communities(
    &mut self,
    pool: &mut DbPool<'_>,
    instance_id: InstanceId,
    last_fetch: DateTime<Utc>,
  ) -> Result<(HashMap<CommunityId, HashSet<Url>>, DateTime<Utc>)> {
    let new_last_fetch = Utc::now() - chrono::Duration::seconds(10); // update to time before fetch to ensure overlap. subtract 10s to ensure overlap even if published date is not exact
    Ok((
      CommunityFollowerView::get_instance_followed_community_inboxes(pool, instance_id, last_fetch)
        .await?
        .into_iter()
        .fold(HashMap::new(), |mut map, (c, u)| {
          map.entry(c).or_default().insert(u.into());
          map
        }),
      new_last_fetch,
    ))
  }
  async fn save_and_send_state(&mut self, pool: &mut DbPool<'_>) -> Result<()> {
    self.last_state_insert = Utc::now();
    FederationQueueState::upsert(pool, &self.state).await?;
    self
      .stats_sender
      .send((self.instance.domain.clone(), self.state.clone()))?;
    Ok(())
  }
}
