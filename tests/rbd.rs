// Copyright (c) 2026 Stelia Ltd
// This project is dual-licensed under Apache 2.0 and MIT terms.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

use futures_util::StreamExt;
use rstest::rstest;
use serial_test::serial;
use tokio::time::interval;

use std::{
    error::Error,
    io::ErrorKind,
    pin::pin,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use async_ceph::{
    async_rt::Tokio,
    buf::BoundedBuf,
    rados::{IoCtx, RadosClientBuilder},
    rbd::{
        AtomicProgressWatcher,
        Features,
        Image,
        ImageBuilder,
        ImageOptions,
        ZeroFlags,
        advisory_locks::{LockError, LockInfo},
        diff::DiffExtent,
        options::StripeLayout,
        snap::{SnapCreateFlags, SnapKey, SnapNamespaceType, SnapRemoveFlags},
        watch::{ImageWatchAsync, ImageWatchCb},
    },
};

mod fixtures;
use fixtures::*;

#[tokio::test]
#[rstest]
#[serial]
async fn create_image(ctx: IoCtx, _cleanup: CleanupOnPanic) {
    ctx.rbd_create(c"test-image", 1024 * 1024 * 4096).await.unwrap();
}

#[tokio::test]
#[rstest]
#[serial]
async fn create_image_features(ctx: IoCtx) -> Result<(), Box<dyn Error>> {
    let expected = Features::LAYERING | Features::EXCLUSIVE_LOCK;

    ctx.rbd_create_with_features(c"test-create-features", 1024 * 1024 * 4096, expected)
        .await?;
    let features = Image::open(&ctx, c"test-create-features").await?.features().await?;
    assert_eq!(features, expected);

    Ok(())
}

#[tokio::test]
#[rstest]
#[serial]
async fn create_image_options(ctx: IoCtx) -> Result<(), Box<dyn Error>> {
    let options = ImageOptions::new().features(Features::LAYERING);

    ctx.rbd_create_with_options(c"test-create-options-set-feats", 1024 * 1024 * 4096, &options)
        .await?;

    let img_features = Image::open(&ctx, c"test-create-options-set-feats").await?.features().await?;
    assert_eq!(img_features, Features::LAYERING);

    let options = options
        .enable_features(Features::JOURNALING)
        .disable_features(Features::DEEP_FLATTEN)
        .stripe_layout(StripeLayout::new(1 << 18, 8))
        .order(20)
        .data_pool(c"rbd".to_owned())
        .journal_pool(c"rbd".to_owned());

    ctx.rbd_create_with_options(c"test-create-options-general", 1024 * 1024, options).await?;

    let img_info = Image::open(&ctx, c"test-create-options-general").await?.full_info().await?;
    tracing::info!("info: {:#?}", img_info);

    assert_eq!(img_info.order, 20);
    assert_eq!(img_info.stripe_unit, 1 << 18);
    assert_eq!(img_info.stripe_count, 8);
    assert!(!img_info.features.contains(Features::DEEP_FLATTEN));
    assert!(img_info.features.contains(Features::JOURNALING));

    Ok(())
}

#[tokio::test]
#[rstest]
#[serial]
async fn open_image(ctx: IoCtx) -> Result<(), Box<dyn Error>> {
    let image = Image::open(&ctx, c"sample.img").await?;
    image.close().await?;

    Ok(())
}

#[tokio::test]
#[rstest]
#[serial]
async fn open_by_id(ctx: IoCtx) -> Result<(), Box<dyn Error>> {
    let images = ctx.rbd_list().await?;

    let id = images.iter().find(|img| img.name() == c"sample.img").unwrap().id();
    let image = ImageBuilder::new(&ctx).id(id)?.open().await?;
    image.close().await?;

    Ok(())
}

#[tokio::test]
#[rstest]
#[serial]
async fn open_read_only(ctx: IoCtx, _cleanup: CleanupOnPanic) -> Result<(), CleanupOnError> {
    let image = ImageBuilder::new(&ctx).read_only(true).name("sample.img")?.open().await?;

    let (_, res) = image.write(0, "test").await;
    println!("{:?}", res);
    assert!(res.is_err());

    Ok(())
}

#[tokio::test]
#[rstest]
#[serial]
async fn read_image(ctx: IoCtx) -> Result<(), Box<dyn Error>> {
    let img = Image::open(&ctx, c"sample.img").await?;

    const EXPECTED: &[u8] = include_bytes!("./sample.img");

    // queue a bunch of read ops and await them concurrently
    let futures = (0 .. 100).map(|_| img.read(0, Vec::with_capacity(64)));

    for f in futures {
        let (buf, n_read) = f.await;
        assert_eq!(EXPECTED, &buf[.. n_read?])
    }

    // try a vectored read with 4 small vecs that can hold 4 elements
    let bufs = (0 .. 4).map(|_| Vec::<u8>::with_capacity(4).slice(0 .. 4)).collect();
    let (bufs, n_read) = img.read_vectored(0, bufs).await;
    let n_read = n_read?;
    let bufs: Vec<_> = bufs.into_iter().map(|b| b.into_inner()).collect();

    tracing::info!("read {n_read}, bufs: {bufs:02x?}");

    let combined = bufs.into_iter().fold(vec![], |mut acc, b| {
        acc.extend(b);
        acc
    });

    assert_eq!(n_read, EXPECTED.len());
    assert_eq!(&combined, EXPECTED);

    img.close().await?;
    Ok(())
}

#[tokio::test]
#[rstest]
#[serial]
async fn resize_image(ctx: IoCtx, _cleanup: CleanupOnPanic) -> Result<(), CleanupOnError> {
    let img = Image::open(&ctx, c"sample.img").await?;

    let initial_size = img.stat().await?.size;

    img.resize(2 * initial_size, false).await?;
    assert_eq!(img.stat().await?.size, 2 * initial_size);

    let err = img.resize(initial_size, false).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidInput);

    img.resize(initial_size, true).await?;

    Ok(())
}

#[tokio::test]
#[rstest]
#[serial]
async fn write_image(ctx: IoCtx, _cleanup: CleanupOnPanic) -> Result<(), CleanupOnError> {
    let img = Image::open(&ctx, c"sample.img").await?;

    // Try to append "test" to the end of the image. Requires resizing!

    const INITIAL: &[u8] = include_bytes!("./sample.img");
    let mut expected = INITIAL.to_owned();
    expected.extend_from_slice(b"test");

    img.resize(expected.len() as u64, false).await?;
    let (_, write_success) = img.write(INITIAL.len() as u64, "test").await;
    write_success?;

    let (buf, n_read) = img.read(0, Vec::with_capacity(64)).await;
    assert_eq!(&expected, &buf[.. n_read?]);

    // Try a vectored write over what we just wrote
    expected[INITIAL.len() ..].copy_from_slice(b"abcd");
    let bufs = vec!["a", "b", "c", "d"];
    let (_, write_success) = img.write_vectored(INITIAL.len() as u64, bufs).await;
    write_success?;

    let (buf, n_read) = img.read(0, Vec::with_capacity(64)).await;
    assert_eq!(&expected, &buf[.. n_read?]);

    // Discard the data that was just written

    img.resize(INITIAL.len() as u64, true).await?;

    let (buf, n_read) = img.read(0, Vec::with_capacity(64)).await;
    assert_eq!(INITIAL, &buf[.. n_read?]);

    img.close().await?;
    Ok(())
}

#[tokio::test]
#[rstest]
#[serial]
async fn writesame_image(ctx: IoCtx) -> Result<(), Box<dyn Error>> {
    ctx.rbd_create(c"test-image-writesame", 256).await?;
    let img = Image::open(&ctx, c"test-image-writesame").await?;

    const PATTERN: &str = "0123456789abcdef";

    let (_, res) = img.write_same(0, 16, PATTERN).await;
    res?;

    let (buf, n_read) = img.read(0, Vec::with_capacity(256)).await;
    let n_read = n_read?;
    assert_eq!(n_read, 256);

    let expected: Vec<u8> = std::iter::repeat_n(PATTERN.as_bytes(), 16).flatten().copied().collect();
    assert_eq!(&buf[.. n_read], expected);

    Ok(())
}

#[tokio::test]
#[rstest]
#[serial]
async fn write_zeroes_image(ctx: IoCtx) -> Result<(), Box<dyn Error>> {
    ctx.rbd_create(c"test-image-writezeroes", 256).await?;
    let img = Image::open(&ctx, c"test-image-writezeroes").await?;

    const PATTERN: &str = "0123456789abcdef";
    let (_, res) = img.write_same(0, 16, PATTERN).await;
    res?;

    img.write_zeroes(0, 256, ZeroFlags::empty()).await?;

    let (buf, n_read) = img.read(0, Vec::with_capacity(256)).await;
    assert_eq!(n_read?, 256);
    assert!(buf.iter().all(|&b| b == 0));

    img.discard(0, 256).await?;

    Ok(())
}

#[tokio::test]
#[rstest]
#[serial]
async fn compare_and_write_image(ctx: IoCtx) -> Result<(), Box<dyn Error>> {
    ctx.rbd_create(c"test-image-cmp-write", 16).await?;
    let img = Image::open(&ctx, c"test-image-cmp-write").await?;

    // normal compare_and_write

    const PATTERN: &str = "0123456789abcdef";
    const INCORRECT: &str = "1234567x9abcdef";

    img.write(0, PATTERN).await.1?;

    let res = img.compare_and_write(1, INCORRECT, vec![0; 15]).await.1?;
    assert_eq!(res, Err(7));

    let res = img.compare_and_write(1, &PATTERN[1 ..], vec![0; 15]).await.1?;
    assert_eq!(res, Ok(()));

    let (buf, n_read) = img.read(1, Vec::with_capacity(15)).await;
    assert_eq!(n_read?, 15);
    assert!(buf.iter().all(|&b| b == 0));

    // vectored compare_and_write

    let incorrect = vec!["89xbcd", "ef"];
    let correct = vec!["89ab", "cde", "f"];

    img.write(0, PATTERN).await.1?;

    let res = img.compare_and_write_vectored(8, incorrect, vec![vec![0; 8]]).await.1?;
    assert_eq!(res, Err(2));

    let res = img.compare_and_write_vectored(8, correct, vec!["........"]).await.1?;
    assert_eq!(res, Ok(()));

    let (buf, n_read) = img.read(0, Vec::with_capacity(16)).await;
    assert_eq!(n_read?, 16);
    assert_eq!(buf, b"01234567........");

    Ok(())
}

#[tokio::test]
#[rstest]
#[serial]
async fn remove_image(ctx: IoCtx, _cleanup: CleanupOnPanic) {
    ctx.rbd_create(c"test-image-remove", 1 << 32).await.unwrap();
    ctx.rbd_remove(c"test-image-remove").await.unwrap();
}

#[tokio::test]
#[rstest]
#[serial]
async fn remove_image_with_progress(ctx: IoCtx, _cleanup: CleanupOnPanic) -> Result<(), CleanupOnError> {
    ctx.rbd_create(c"test-image-remove-with-progress", 1 << 43).await?;

    let progress = AtomicProgressWatcher::new();
    let mut removal_done = pin!(ctx.rbd_remove(c"test-image-remove-with-progress").with_progress(progress.clone()));

    // update progress 20/sec
    let mut interval = interval(Duration::from_millis(50));
    loop {
        tokio::select! {
            _ = interval.tick() => match progress.latest() {
                p if !p.started() => continue,
                p => tracing::info!("removal progress: {:.01} %", 100.0 * p.as_fraction()),
            },
            result = removal_done.as_mut() => break result
        }
    }?;
    Ok(())
}

#[tokio::test]
#[rstest]
#[serial]
async fn image_info(ctx: IoCtx) -> Result<(), Box<dyn Error>> {
    let images = ctx.rbd_list().await?;
    tracing::info!("images: {:?}", images);

    assert!(images.iter().any(|img| img.name() == c"sample.img"));

    let image = Image::open(&ctx, c"sample.img").await?;

    tracing::info!("info: {:#?}", image.full_info().await?);
    tracing::info!("group: {:#?}", image.group().await?);
    tracing::info!("parent: {:#?}", image.parent().await?);
    tracing::info!("children: {:#?}", image.children().await?);
    tracing::info!("descendants: {:#?}", image.descendants().await?);
    tracing::info!("watchers: {:#?}", image.watchers().await?);
    tracing::info!("snapshots: {:#?}", image.snap_list().await?);

    Ok(())
}

#[tokio::test]
#[rstest]
#[serial]
async fn watch_image(ctx: IoCtx) -> Result<(), Box<dyn Error>> {
    ctx.rbd_create(c"watch-test", 1024).await?;

    let image_for_watch = Image::open(&ctx, c"watch-test").await?;
    let image_for_changes = Image::open(&ctx, c"watch-test").await?;

    static CB_UPDATES: AtomicUsize = AtomicUsize::new(0);
    let _cb_watch = ImageWatchCb::new(&image_for_watch, || {
        CB_UPDATES.fetch_add(1, Ordering::SeqCst);
        tracing::info!("image updated (cb)");
    })?;

    let mut async_watch_updates = 0;
    let mut async_watch = ImageWatchAsync::new(&image_for_watch)?;

    let mut modifications = pin!(async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        image_for_changes.resize(2048, false).await.unwrap();

        tokio::time::sleep(Duration::from_secs(1)).await;
        image_for_changes.resize(4096, false).await.unwrap();

        tokio::time::sleep(Duration::from_secs(1)).await;
        image_for_changes.resize(8096, false).await.unwrap();
    });

    loop {
        tokio::select! {
            _ = async_watch.wait() => {
                async_watch_updates += 1;
                tracing::info!("image updated (async)");
            },
            _ = &mut modifications => break,
        };
    }

    assert_eq!(CB_UPDATES.load(Ordering::SeqCst), 3);
    assert_eq!(async_watch_updates, 3);

    Ok(())
}

#[tokio::test]
#[rstest]
#[serial]
async fn image_snapshots(ctx: IoCtx, _cleanup: CleanupOnPanic) -> Result<(), CleanupOnError> {
    const SNAP1: &str = "0123456789";
    const SNAP3: &str = "abcdefghij";
    const HEAD: &str = "----------";

    ctx.rbd_create(c"test-snapshots", SNAP1.len() as u64).await?;

    let image = Image::open(&ctx, c"test-snapshots").await?;
    image.write(0, SNAP1).await.1?;

    image.snap_create("snap1", SnapCreateFlags::NONE).await?;
    image.snap_create("snap2", SnapCreateFlags::NONE).await?;

    image.write(0, "abcdefghij").await.1?;
    image.snap_create("snap3", SnapCreateFlags::NONE).await?;

    let snap_infos = image.snap_list().await?;
    tracing::info!("snapshots: {:#?}", snap_infos);

    assert_eq!(snap_infos.len(), 3);
    assert_eq!(snap_infos[0].name(), c"snap1");
    assert_eq!(snap_infos[1].name(), c"snap2");
    assert_eq!(snap_infos[2].name(), c"snap3");

    let snap1 = image.snap_by_id(snap_infos[0].id());
    let mut snap2 = image.snap_by_name("snap2")?;

    assert!(snap1.exists().await?);
    assert!(snap2.exists().await?);
    assert!(!image.snap_exists(c"snap4".into()).await?);
    assert!(!image.snap_exists(0xDEADBEEF.into()).await?);

    assert_eq!(snap1.name().await?, snap_infos[0].name());
    assert_eq!(snap2.id().await?, snap_infos[1].id());

    tracing::info!("{:?}", snap1.timestamp().await?);
    tracing::info!("{:?}", snap1.namespace_type().await?);
    tracing::info!("{:?}", snap1.namespace().await?);

    // Try to rename snap2
    snap2.rename("snap2-new").await?;
    assert_eq!(snap2.name(), c"snap2-new");

    // Check protection effect on snapshot remove

    assert!(!snap2.protect(true).await?);
    assert!(snap2.protect(true).await?);

    let remove_err = snap2.remove(SnapRemoveFlags::NONE).await.unwrap_err();
    assert_eq!(remove_err.kind(), ErrorKind::ResourceBusy);

    assert!(snap2.protect(false).await?);
    snap2.remove(SnapRemoveFlags::NONE).await?;

    // Check rbd_set effect on reads

    let (buf, n_read) = image.read(0, Vec::with_capacity(SNAP3.len())).await;
    assert_eq!(&buf[.. n_read?], SNAP3.as_bytes());

    image.snap_set(&snap1).await?;
    let (buf, n_read) = image.read(0, Vec::with_capacity(SNAP1.len())).await;
    assert_eq!(&buf[.. n_read?], SNAP1.as_bytes());

    image.snap_set(c"snap3").await?;

    // Write to head through another Image, should not be visible after snap_set
    let head_view = Image::open(&ctx, "test-snapshots").await?;
    head_view.write(0, HEAD).await.1?;
    head_view.close().await?;

    let (buf, n_read) = image.read(0, Vec::with_capacity(SNAP3.len())).await;
    assert_eq!(&buf[.. n_read?], SNAP3.as_bytes());

    // Should become visible when image is pointed to head again
    image.snap_set(SnapKey::HEAD).await?;
    let (buf, n_read) = image.read(0, Vec::with_capacity(HEAD.len())).await;
    assert_eq!(&buf[.. n_read?], HEAD.as_bytes());

    // Check that image can be opened as a snapshot
    let snap_view = ImageBuilder::new(&ctx).name("test-snapshots")?.snap_name("snap1")?.open().await?;
    let (buf, n_read) = snap_view.read(0, Vec::with_capacity(SNAP1.len())).await;
    assert_eq!(&buf[.. n_read?], SNAP1.as_bytes());
    snap_view.close().await?;

    // Rollback the image to snap3
    image.snap_rollback("snap3").await?;
    let (buf, n_read) = image.read(0, Vec::with_capacity(SNAP3.len())).await;
    assert_eq!(&buf[.. n_read?], SNAP3.as_bytes());

    // Protect the snapshot (required for clone to succeed)
    snap1.protect(true).await?;

    // Clone snap1 to a new image
    ctx.rbd_clone_within("test-snapshots", c"snap1", "test-snapshots-clone", ImageOptions::new())
        .await?;

    let clone = Image::open(&ctx, "test-snapshots-clone").await?;
    let parent = clone.parent().await?.unwrap();
    tracing::info!("{:?}", parent);

    assert_eq!(parent.image.image_name(), c"test-snapshots");
    assert_eq!(parent.image.pool_name(), c"rbd");
    assert_eq!(parent.snap.id(), snap1.id());
    assert_eq!(parent.snap.name(), &snap1.name().await?);
    assert_eq!(parent.snap.namespace_type(), SnapNamespaceType::User);

    Ok(())
}

#[tokio::test]
#[rstest]
#[serial]
async fn advisory_locks(ctx: IoCtx, _cleanup: CleanupOnPanic) -> Result<(), CleanupOnError> {
    let image = Image::open(&ctx, c"sample.img").await?;

    // No locks on the image, info() should return Ok(None)
    assert!(image.advisory_lock().info().await?.is_none());

    let shared1 = image.advisory_lock().cookie("cookie1")?.acquire_shared("foo").await??;
    assert_eq!(shared1.cookie(), c"cookie1");
    assert_eq!(shared1.tag(), c"foo");

    // acquire lock held by self (should fail)
    let res = image.advisory_lock().cookie("cookie1")?.acquire_shared("foo").await?;
    assert!(matches!(res, Err(LockError::HeldBySelf)));

    // acquire exclusively (this should fail)
    let res = image.advisory_lock().acquire().await?;
    assert!(matches!(res, Err(LockError::HeldByOther)));

    // acquire shared lock with different tag (should fail)
    let res = image.advisory_lock().acquire_shared("bar").await?;
    assert!(matches!(res, Err(LockError::HeldByOther)));

    // acquire a second shared lock with same tag and different cookie (works)
    let shared2 = image.advisory_lock().cookie("cookie2")?.acquire_shared("foo").await??;
    assert_eq!(shared2.cookie(), c"cookie2");
    assert_eq!(shared2.tag(), c"foo");

    // get the currently held lock information
    let lock_info = image.advisory_lock().info().await?;
    tracing::info!("{lock_info:#?}");
    let Some(LockInfo::Shared { tag, lockers }) = lock_info else {
        panic!("expected shared lock info")
    };

    assert_eq!(tag, c"foo");
    let locker_cookies: Vec<_> = lockers.iter().map(|l| l.cookie.as_c_str()).collect();
    assert_eq!(locker_cookies.len(), 2);
    assert!(locker_cookies.contains(&c"cookie1"));
    assert!(locker_cookies.contains(&c"cookie2"));

    // release the locks and try to acquire and exclusive lock
    shared1.unlock().await?;
    shared2.unlock().await?;

    let exclusive = image.advisory_lock().acquire().await??;
    tracing::info!("cookie: {:?}", exclusive.cookie());

    // check held lock information again
    let Some(LockInfo::Exclusive(locker)) = image.advisory_lock().info().await? else {
        panic!("expected exclusive lock info")
    };
    assert_eq!(locker.cookie, exclusive.cookie());

    // unlock through drop
    drop(exclusive);

    // try to forcefully break a lock
    //
    // properly testing this requires creating a separate client so we can keep communicating
    // with the cluster after it gets blacklisted

    let client2 = RadosClientBuilder::default()
        .executor(&Tokio)
        .load_config_file("./ceph.conf")?
        .connect()
        .await?;

    let ctx2 = client2.create_io_ctx("rbd").await?;
    let image2 = Image::open(&ctx2, c"sample.img").await?;

    // acquire a lock on the second client
    let exclusive = image2.advisory_lock().acquire().await??;

    // acquiring a lock on the original client should fail (second client holds it)
    let res = image.advisory_lock().acquire().await?;
    assert!(matches!(res, Err(LockError::HeldByOther)));

    // forcefully break the lock held by the original client

    let Some(LockInfo::Exclusive(locker)) = image.advisory_lock().info().await? else {
        panic!("expected exclusive lock info")
    };

    let broken = image.advisory_lock().break_lock(&locker).await?;
    assert!(broken);

    // try it again (lock is no longer held, so should return false)
    let broken = image.advisory_lock().break_lock(&locker).await?;
    assert!(!broken);

    // we should now be able to acquire a lock on the original client
    image.advisory_lock().acquire().await??;

    // unlocking `exclusive` will fail since `client2` is now blacklisted.
    exclusive.unlock().await.unwrap_err();

    Ok(())
}

const SAMPLE_IMG_EXTENT: DiffExtent = DiffExtent {
    offset: 0,
    len: 10,
    exists: true,
};

#[tokio::test]
#[rstest]
#[serial]
async fn image_diff_simple(ctx: IoCtx) -> Result<(), Box<dyn Error>> {
    let image = Image::open(&ctx, "sample.img").await?;

    let full_diff = image.diff_iterate().collect().await?;
    assert_eq!(full_diff, [SAMPLE_IMG_EXTENT]);

    let mut diff_stream = image.diff_iterate().stream();
    assert!(matches!(diff_stream.next().await, Some(Ok(SAMPLE_IMG_EXTENT))));
    assert!(diff_stream.next().await.is_none());

    let used_between_5_and_20 = image
        .diff_iterate()
        .offset(5)
        .len(15)
        .fold(0, |state, diff| {
            if diff.exists {
                *state += diff.len
            }
        })
        .await?;

    assert_eq!(used_between_5_and_20, 5);

    // to test error cases try to diff from a non-existent snapshot
    let bad_snapshot = c"snap-that-does-not-exist";

    image.diff_iterate().from_snap(bad_snapshot).collect().await.unwrap_err();

    let mut err_expected = image.diff_iterate().from_snap(bad_snapshot).stream();
    assert!(matches!(err_expected.next().await, Some(Err(_))));
    assert!(err_expected.next().await.is_none());

    Ok(())
}

#[tokio::test]
#[rstest]
#[serial]
async fn image_diff_parent(ctx: IoCtx, _cleanup: CleanupOnPanic) -> Result<(), CleanupOnError> {
    // needs to be larger than the image order, otherwise extents will be merged
    const OFFSET: u64 = 16 * 1024u64.pow(2);

    const NEW_EXTENT: DiffExtent = DiffExtent {
        offset: OFFSET,
        len: 6,
        exists: true,
    };

    // create a clone of sample.img
    let parent_image = Image::open(&ctx, c"sample.img").await?;
    parent_image.snap_create(c"image-diff-test-snap", SnapCreateFlags::empty()).await?;
    parent_image.snap_by_name(c"image-diff-test-snap").unwrap().protect(true).await?;
    ctx.rbd_clone_within(c"sample.img", c"image-diff-test-snap", c"diff-parent-clone", ImageOptions::new())
        .await?;

    // resize the clone and add additional data
    let image = Image::open(&ctx, c"diff-parent-clone").await?;
    image.resize(1024u64.pow(3), false).await?;
    image.write(OFFSET, "abcdef").await.1?;

    // should show all extents, including those from parent
    let with_parent = image.diff_iterate().include_parent().collect().await?;
    tracing::info!("diff (including parent): {with_parent:?}");
    assert_eq!(with_parent, [SAMPLE_IMG_EXTENT, NEW_EXTENT]);

    // should only show extents from snapshot (the new write)
    let without_parent = image.diff_iterate().collect().await?;
    tracing::info!("diff (without parent): {without_parent:?}");
    assert_eq!(without_parent, [NEW_EXTENT]);

    // create a new snapshot where everything was zero'd out
    image.snap_create(c"image-diff-test-child-snap", SnapCreateFlags::empty()).await?;
    image
        .write_zeroes(SAMPLE_IMG_EXTENT.offset, SAMPLE_IMG_EXTENT.len, ZeroFlags::empty())
        .await?;
    image.write_zeroes(NEW_EXTENT.offset, NEW_EXTENT.len, ZeroFlags::empty()).await?;

    // should show that the extents have been removed
    let diff = image.diff_iterate().from_snap(c"image-diff-test-child-snap").collect().await?;
    tracing::info!("diff (post zero): {diff:?}");
    assert_eq!(diff, [SAMPLE_IMG_EXTENT, NEW_EXTENT].map(|d| DiffExtent { exists: false, ..d }));

    image.close().await?;
    parent_image.close().await?;

    Ok(())
}
