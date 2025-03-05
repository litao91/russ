//! This module provides a way to asynchronously refresh feeds, using threads

use crate::ReadOptions;
use crate::app::App;
use crate::modes::Mode;
use anyhow::Result;
use tokio::sync::mpsc::UnboundedSender;

pub(crate) enum Action {
    RefreshFeed(crate::rss::FeedId),
    RefreshFeeds(Vec<crate::rss::FeedId>),
    SubscribeToFeed(String),
    ClearFlash,
}

/// A loop to process `io::Action` messages.
pub(crate) async fn io_loop(
    app: &App,
    io_tx: &UnboundedSender<Action>,
    options: &ReadOptions,
    event: Action,
) -> Result<()> {
    let manager = r2d2_sqlite::SqliteConnectionManager::file(&options.database_path);
    let connection_pool = r2d2::Pool::new(manager)?;

    match event {
        Action::RefreshFeed(feed_id) => {
            let now = std::time::Instant::now();

            app.set_flash("Refreshing feed...".to_string()).await;
            app.force_redraw().await?;

            refresh_feeds(
                &app,
                &connection_pool,
                &[feed_id],
                async |_app, fetch_result| {
                    if let Err(e) = fetch_result {
                        app.push_error_flash(e).await;
                    }
                },
            )
            .await?;

            app.update_current_feed_and_entries().await?;
            let elapsed = now.elapsed();
            app.set_flash(format!("Refreshed feed in {elapsed:?}"))
                .await;
            app.force_redraw().await?;
            clear_flash_after(io_tx.clone(), options.flash_display_duration_seconds);
        }
        Action::RefreshFeeds(feed_ids) => {
            let now = std::time::Instant::now();

            app.set_flash("Refreshing all feeds...".to_string()).await;
            app.force_redraw().await?;

            let all_feeds_len = feed_ids.len();
            let mut successfully_refreshed_len = 0usize;

            refresh_feeds(
                &app,
                &connection_pool,
                &feed_ids,
                async |app, fetch_result| match fetch_result {
                    Ok(_) => successfully_refreshed_len += 1,
                    Err(e) => app.push_error_flash(e).await,
                },
            )
            .await?;

            {
                app.update_current_feed_and_entries().await?;

                let elapsed = now.elapsed();
                app.set_flash(format!(
                    "Refreshed {successfully_refreshed_len}/{all_feeds_len} feeds in {elapsed:?}"
                ))
                .await;
                app.force_redraw().await?;
            }

            clear_flash_after(io_tx.clone(), options.flash_display_duration_seconds);
        }
        Action::SubscribeToFeed(feed_subscription_input) => {
            let now = std::time::Instant::now();

            app.set_flash("Subscribing to feed...".to_string()).await;
            app.force_redraw().await?;

            let mut conn = connection_pool.get()?;
            let r = crate::rss::subscribe_to_feed(
                &app.http_client().await,
                &mut conn,
                &feed_subscription_input,
            )
            .await;

            if let Err(e) = r {
                app.push_error_flash(e).await;
                return Ok(());
            }

            match crate::rss::get_feeds(&conn) {
                Ok(feeds) => {
                    {
                        app.reset_feed_subscription_input().await;
                        app.set_feeds(feeds).await;
                        app.select_feeds().await;
                        app.update_current_feed_and_entries().await?;

                        let elapsed = now.elapsed();
                        app.set_flash(format!("Subscribed in {elapsed:?}")).await;
                        app.set_mode(Mode::Normal).await;
                        app.force_redraw().await?;
                    }

                    clear_flash_after(io_tx.clone(), options.flash_display_duration_seconds);
                }
                Err(e) => {
                    app.push_error_flash(e).await;
                }
            }
        }
        Action::ClearFlash => {
            app.clear_flash().await;
        }
    }

    Ok(())
}

/// Refreshes the feeds of the given `feed_ids` by splitting them into
/// chunks based on the number of available CPUs.
/// Each chunk is then passed to its own thread,
/// where each feed_id in the chunk has its feed refreshed synchronously on that thread.
async fn refresh_feeds<F>(
    app: &App,
    connection_pool: &r2d2::Pool<r2d2_sqlite::SqliteConnectionManager>,
    feed_ids: &[crate::rss::FeedId],
    mut refresh_result_handler: F,
) -> Result<()>
where
    F: AsyncFnMut(&App, anyhow::Result<()>),
{
    let chunks = chunkify_for_threads(feed_ids, num_cpus::get() * 2);

    let http_client = app.http_client().await;
    let join_handles: Vec<_> = chunks
        .map(|chunk| {
            let pool_get_result = connection_pool.get();
            let chunk = chunk.to_owned();
            let hc = http_client.clone();

            tokio::spawn(async move {
                let mut conn = pool_get_result?;
                let mut results = Vec::new();
                for feed_id in chunk {
                    results.push(crate::rss::refresh_feed(&hc, &mut conn, feed_id).await);
                }

                Ok::<Vec<Result<(), anyhow::Error>>, anyhow::Error>(results)
            })
        })
        .collect();

    for join_handle in join_handles {
        let chunk_results = join_handle
            .await
            .expect("unable to join worker thread to io thread");
        for chunk_result in chunk_results? {
            refresh_result_handler(app, chunk_result).await;
        }
    }

    Ok(())
}

/// split items into chunks,
/// with the idea being that each chunk will be run on its own thread
fn chunkify_for_threads<T>(
    items: &[T],
    minimum_number_of_threads: usize,
) -> impl Iterator<Item = &[T]> {
    // example: 25 items / 16 threads = chunk size of 1
    // example: 100 items / 16 threads = chunk size of 6
    // example: 10 items / 16 threads = chunk size of 0 (handled later)
    //
    // due to usize floor division, it's possible chunk_size would be 0,
    // so ensure it is at least 1
    let chunk_size = (items.len() / minimum_number_of_threads).max(1);

    // now we have (len / chunk_size) chunks,
    // example:
    // 25 items / chunks size of 1 = 25 chunks
    // 100 items / chunk size of 6 = 16 chunks
    items.chunks(chunk_size)
}

/// clear the flash after a given duration
fn clear_flash_after(tx: UnboundedSender<Action>, duration: std::time::Duration) {
    tokio::spawn(async move {
        tokio::time::sleep(duration).await;
        tx.send(Action::ClearFlash)
            .expect("Unable to send IOCommand::ClearFlash");
    });
}
