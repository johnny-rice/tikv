// Copyright 2021 TiKV Project Authors. Licensed under Apache-2.0.

use std::{
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

use api_version::{KvFormat, test_kv_format_impl};
use causal_ts::CausalTsProvider;
use cdc::{Delegate, OldValueCache, Task, Validate, recv_timeout};
use futures::{executor::block_on, sink::SinkExt};
use grpcio::{ChannelBuilder, Environment, WriteFlags};
use kvproto::{cdcpb::*, kvrpcpb::*, tikvpb_grpc::TikvClient};
use pd_client::PdClient;
use test_raftstore::*;
use tikv_util::{HandyRwLock, debug, worker::Scheduler};
use txn_types::{Key, TimeStamp};

use crate::{ClientReceiver, TestSuite, TestSuiteBuilder, new_event_feed, new_event_feed_v2};

#[test]
fn test_cdc_double_scan_deregister() {
    test_kv_format_impl!(test_cdc_double_scan_deregister_impl<ApiV1 ApiV2>);
}

fn test_cdc_double_scan_deregister_impl<F: KvFormat>() {
    let mut suite = TestSuite::new(1, F::TAG);

    // If tikv enable ApiV2, txn key needs to start with 'x';
    let (k, v) = (b"xkey1".to_vec(), b"value".to_vec());
    // Prewrite
    let start_ts1 = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    let mut mutation = Mutation::default();
    mutation.set_op(Op::Put);
    mutation.key = k.clone();
    mutation.value = v;
    suite.must_kv_prewrite(1, vec![mutation], k.clone(), start_ts1);
    // Commit
    let commit_ts1 = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    suite.must_kv_commit(1, vec![k], start_ts1, commit_ts1);

    let fp = "cdc_incremental_scan_start";
    fail::cfg(fp, "pause").unwrap();

    let mut req = suite.new_changedata_request(1);
    req.mut_header().set_ticdc_version("5.0.0".into());
    let (mut req_tx, event_feed_wrap, _receive_event) =
        new_event_feed(suite.get_region_cdc_client(1));
    block_on(req_tx.send((req, WriteFlags::default()))).unwrap();

    // wait for the first connection to start incremental scan
    sleep_ms(1000);

    let mut req = suite.new_changedata_request(1);
    req.mut_header().set_ticdc_version("5.0.0".into());
    let (mut req_tx_1, event_feed_wrap_1, receive_event_1) =
        new_event_feed(suite.get_region_cdc_client(1));
    block_on(req_tx_1.send((req, WriteFlags::default()))).unwrap();

    // wait for the second connection register to the delegate.
    suite.must_wait_delegate_condition(
        1,
        1,
        Arc::new(|d: Option<&Delegate>| d.unwrap().downstreams().len() == 2),
    );

    // close connection
    block_on(req_tx.close()).unwrap();
    event_feed_wrap.replace(None);

    // wait for the connection to close
    sleep_ms(300);

    fail::cfg(fp, "off").unwrap();

    'outer: loop {
        let event = receive_event_1(false);
        let mut event_vec = event.events.to_vec();
        while !event_vec.is_empty() {
            if let Event_oneof_event::Error(err) = event_vec.pop().unwrap().event.unwrap() {
                assert!(err.has_region_not_found(), "{:?}", err);
                break 'outer;
            }
        }
    }

    event_feed_wrap_1.replace(None);
    suite.stop();
}

#[test]
fn test_cdc_double_scan_io_error() {
    test_kv_format_impl!(test_cdc_double_scan_io_error_impl<ApiV1 ApiV2>);
}

fn test_cdc_double_scan_io_error_impl<F: KvFormat>() {
    let mut suite = TestSuite::new(1, F::TAG);

    let (k, v) = (b"xkey1".to_vec(), b"value".to_vec());
    // Prewrite
    let start_ts1 = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    let mut mutation = Mutation::default();
    mutation.set_op(Op::Put);
    mutation.key = k.clone();
    mutation.value = v;
    suite.must_kv_prewrite(1, vec![mutation], k.clone(), start_ts1);
    // Commit
    let commit_ts1 = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    suite.must_kv_commit(1, vec![k], start_ts1, commit_ts1);

    fail::cfg("cdc_incremental_scan_start", "pause").unwrap();
    fail::cfg("cdc_scan_batch_fail", "1*return").unwrap();

    let mut req = suite.new_changedata_request(1);
    req.mut_header().set_ticdc_version("5.0.0".into());
    let (mut req_tx, event_feed_wrap, receive_event) =
        new_event_feed(suite.get_region_cdc_client(1));
    block_on(req_tx.send((req, WriteFlags::default()))).unwrap();

    // wait for the first connection to start incremental scan
    sleep_ms(1000);

    let mut req = suite.new_changedata_request(1);
    req.mut_header().set_ticdc_version("5.0.0".into());
    let (mut req_tx_1, event_feed_wrap_1, receive_event_1) =
        new_event_feed(suite.get_region_cdc_client(1));
    block_on(req_tx_1.send((req, WriteFlags::default()))).unwrap();

    // wait for the second connection to start incremental scan
    sleep_ms(1000);

    // unpause
    fail::cfg("cdc_incremental_scan_start", "off").unwrap();

    'outer: loop {
        let event = receive_event(true);
        if let Some(resolved_ts) = event.resolved_ts.as_ref() {
            // resolved ts has been pushed, so this conn is good.
            if resolved_ts.regions.len() == 1 {
                break 'outer;
            }
        }
        let mut event_vec = event.events.to_vec();
        while !event_vec.is_empty() {
            match event_vec.pop().unwrap().event.unwrap() {
                Event_oneof_event::Error(err) => {
                    assert!(err.has_region_not_found(), "{:?}", err);
                    break 'outer;
                }
                e => {
                    debug!("cdc receive_event {:?}", e);
                }
            }
        }
    }

    'outer1: loop {
        let event = receive_event_1(true);
        if let Some(resolved_ts) = event.resolved_ts.as_ref() {
            // resolved ts has been pushed, so this conn is good.
            if resolved_ts.regions.len() == 1 {
                break 'outer1;
            }
        }
        let mut event_vec = event.events.to_vec();
        while !event_vec.is_empty() {
            match event_vec.pop().unwrap().event.unwrap() {
                Event_oneof_event::Error(err) => {
                    assert!(err.has_region_not_found(), "{:?}", err);
                    break 'outer1;
                }
                e => {
                    debug!("cdc receive_event {:?}", e);
                }
            }
        }
    }

    event_feed_wrap.replace(None);
    event_feed_wrap_1.replace(None);
    suite.stop();
}

#[test]
#[ignore = "TODO: support continue scan after region split"]
fn test_cdc_scan_continues_after_region_split() {
    test_kv_format_impl!(test_cdc_scan_continues_after_region_split_impl<ApiV1 ApiV2>);
}

fn test_cdc_scan_continues_after_region_split_impl<F: KvFormat>() {
    fail::cfg("cdc_after_incremental_scan_blocks_regional_errors", "pause").unwrap();

    let mut suite = TestSuite::new(1, F::TAG);

    let (k, v) = (b"xkey1".to_vec(), b"value".to_vec());
    // Prewrite
    let start_ts1 = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    let mut mutation = Mutation::default();
    mutation.set_op(Op::Put);
    mutation.key = k.clone();
    mutation.value = v;
    suite.must_kv_prewrite(1, vec![mutation], k.clone(), start_ts1);
    // Commit
    let commit_ts1 = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    suite.must_kv_commit(1, vec![k], start_ts1, commit_ts1);

    let mut req = suite.new_changedata_request(1);
    req.mut_header().set_ticdc_version("5.0.0".into());
    let (mut req_tx, event_feed_wrap, receive_event) =
        new_event_feed(suite.get_region_cdc_client(1));
    block_on(req_tx.send((req, WriteFlags::default()))).unwrap();

    // wait for the first connection to start incremental scan
    sleep_ms(1000);

    let region = suite.cluster.get_region(b"xkey1");
    suite.cluster.must_split(&region, b"xkey2");

    // wait for region split to be processed
    sleep_ms(1000);

    fail::cfg("cdc_after_incremental_scan_blocks_regional_errors", "off").unwrap();

    let events = receive_event(false).events.to_vec();
    assert_eq!(events.len(), 1, "{:?}", events);
    match events[0].event.as_ref().unwrap() {
        Event_oneof_event::Entries(es) => {
            assert!(es.entries.len() == 2, "{:?}", es);
            let e = &es.entries[0];
            assert_eq!(e.get_type(), EventLogType::Committed, "{:?}", es);
            assert_eq!(e.start_ts, start_ts1.into_inner(), "{:?}", es);
            assert_eq!(e.commit_ts, commit_ts1.into_inner(), "{:?}", es);
            assert_eq!(e.key, b"xkey1", "{:?}", es);
            assert_eq!(e.value, b"value", "{:?}", es);

            let e = &es.entries[1];
            assert_eq!(e.get_type(), EventLogType::Initialized, "{:?}", es);
        }
        other => panic!("unexpected event {:?}", other),
    }

    let event = receive_event(true);
    if let Some(resolved_event) = event.resolved_ts.as_ref() {
        assert_eq!(resolved_event.regions, &[1], "{:?}", resolved_event);
        assert!(
            resolved_event.ts >= commit_ts1.into_inner(),
            "resolved_event: {:?}, commit_ts1: {:?}",
            resolved_event,
            commit_ts1
        );
    } else {
        panic!("unexpected event {:?}", event);
    }

    let events = receive_event(false).events.to_vec();
    match events[0].event.as_ref().unwrap() {
        Event_oneof_event::Error(err) => {
            assert!(err.has_epoch_not_match(), "{:?}", err);
        }
        other => panic!("unexpected event {:?}", other),
    }

    event_feed_wrap.replace(None);
    suite.stop();
}

// Test the `ResolvedTs` sequence shouldn't be pushed to a region downstream
// if the downstream hasn't been initialized.
#[test]
fn test_no_resolved_ts_before_downstream_initialized() {
    for version in &["4.0.7", "4.0.8"] {
        do_test_no_resolved_ts_before_downstream_initialized(version);
    }
}

fn do_test_no_resolved_ts_before_downstream_initialized(version: &str) {
    let cluster = new_server_cluster(0, 1);
    cluster.pd_client.disable_default_operator();
    let mut suite = TestSuiteBuilder::new().cluster(cluster).build();
    let region = suite.cluster.get_region(b"");

    let recv_resolved_ts = |event_feed: &ClientReceiver| {
        let mut rx = event_feed.replace(None).unwrap();
        let timeout = Duration::from_secs(1);
        for _ in 0..10 {
            if let Ok(Some(event)) = recv_timeout(&mut rx, timeout) {
                let event = event.unwrap();
                if event.has_resolved_ts() {
                    event_feed.replace(Some(rx));
                    return;
                }
                for e in event.get_events() {
                    if let Some(Event_oneof_event::ResolvedTs(_)) = e.event {
                        event_feed.replace(Some(rx));
                        return;
                    }
                }
            }
        }
        panic!("must receive a resolved ts");
    };

    // Create 2 changefeeds and the second will be blocked in initialization.
    let mut req_txs = Vec::with_capacity(2);
    let mut event_feeds = Vec::with_capacity(2);
    for i in 0..2 {
        if i == 1 {
            // Wait the first capture has been initialized.
            recv_resolved_ts(&event_feeds[0]);
            fail::cfg("cdc_incremental_scan_start", "pause").unwrap();
        }
        let (mut req_tx, event_feed, _) = new_event_feed(suite.get_region_cdc_client(region.id));
        let mut req = suite.new_changedata_request(region.id);
        req.mut_header().set_ticdc_version(version.to_owned());
        block_on(req_tx.send((req, WriteFlags::default()))).unwrap();
        req_txs.push(req_tx);
        event_feeds.push(event_feed);
    }

    let th = thread::spawn(move || {
        // The first downstream can receive timestamps but the second should receive
        // nothing.
        let mut rx = event_feeds[0].replace(None).unwrap();
        recv_timeout(&mut rx, Duration::from_secs(1)).unwrap();
        let mut rx = event_feeds[1].replace(None).unwrap();
        recv_timeout(&mut rx, Duration::from_secs(3)).unwrap_err();
    });

    th.join().unwrap();
    fail::cfg("cdc_incremental_scan_start", "off").unwrap();
    suite.stop();
}

// When a new CDC downstream is installed, delta changes for other downstreams
// on the same region should be flushed so that the new downstream can gets a
// fresh snapshot to performs a incremental scan. CDC can ensure that those
// delta changes are sent to CDC's `Endpoint` before the incremental scan, but
// `Sink` may break this rule. This case tests it won't happen any more.
#[test]
fn test_cdc_observed_before_incremental_scan_snapshot() {
    let cluster = new_server_cluster(0, 1);
    cluster.pd_client.disable_default_operator();
    let mut suite = TestSuiteBuilder::new().cluster(cluster).build();
    let region = suite.cluster.get_region(b"");
    let lead_client = PeerClient::new(&suite.cluster, region.id, new_peer(1, 1));

    // So that the second changefeed can get some delta changes elder than its
    // snapshot.
    let (mut req_tx_0, event_feed_0, _) = new_event_feed(suite.get_region_cdc_client(region.id));
    let req_0 = suite.new_changedata_request(region.id);
    block_on(req_tx_0.send((req_0, WriteFlags::default()))).unwrap();

    fail::cfg("cdc_before_handle_multi_batch", "pause").unwrap();
    fail::cfg("cdc_sleep_before_drain_change_event", "return").unwrap();
    let (mut req_tx, event_feed, receive_event) =
        new_event_feed(suite.get_region_cdc_client(region.id));
    let req = suite.new_changedata_request(region.id);
    block_on(req_tx.send((req, WriteFlags::default()))).unwrap();
    thread::sleep(Duration::from_secs(1));

    for version in 0..10 {
        let key = format!("key-{:0>6}", version);
        let start_ts = get_tso(&suite.cluster.pd_client);
        lead_client.must_kv_prewrite(
            vec![new_mutation(Op::Put, key.as_bytes(), b"value")],
            key.as_bytes().to_owned(),
            start_ts,
        );
        let commit_ts = get_tso(&suite.cluster.pd_client);
        lead_client.must_kv_commit(vec![key.into_bytes()], start_ts, commit_ts);
    }

    fail::cfg("cdc_before_handle_multi_batch", "off").unwrap();
    fail::cfg("cdc_before_drain_change_event", "off").unwrap();
    // Wait the client wake up from `cdc_sleep_before_drain_change_event`.
    thread::sleep(Duration::from_secs(5));

    // `Initialized` should be the last event.
    let (mut initialized_pos, mut row_count) = (0, 0);
    while initialized_pos == 0 {
        for event in receive_event(false).get_events() {
            if let Some(Event_oneof_event::Entries(ref entries)) = event.event {
                for row in entries.get_entries() {
                    row_count += 1;
                    if row.r_type == EventLogType::Initialized {
                        initialized_pos = row_count;
                    }
                }
            }
        }
    }
    assert!(initialized_pos > 0);
    assert_eq!(initialized_pos, row_count);
    let mut rx = event_feed.replace(None).unwrap();
    if let Ok(Some(Ok(event))) = recv_timeout(&mut rx, Duration::from_secs(1)) {
        assert!(event.get_events().is_empty());
    }

    drop(event_feed_0);
    drop(event_feed);
    suite.stop();
}

#[test]
fn test_old_value_cache_without_downstreams() {
    fn check_old_value_cache(scheduler: &Scheduler<Task>, updates: usize) {
        let (tx, rx) = mpsc::sync_channel(1);
        let checker = move |c: &OldValueCache| tx.send(c.update_count()).unwrap();
        scheduler
            .schedule(Task::Validate(Validate::OldValueCache(Box::new(checker))))
            .unwrap();
        assert_eq!(rx.recv().unwrap(), updates);
    }

    let mutation = || {
        let mut mutation = Mutation::default();
        mutation.set_op(Op::Put);
        mutation.key = b"key".to_vec();
        mutation.value = b"value".to_vec();
        mutation
    };

    fail::cfg("cdc_flush_old_value_metrics", "return").unwrap();

    let cluster = new_server_cluster(0, 1);
    let mut suite = TestSuiteBuilder::new().cluster(cluster).build();
    let scheduler = suite.endpoints[&1].scheduler();

    // Add a subscription and then check old value cache.
    let (mut req_tx, event_feed, receive_event) = new_event_feed(suite.get_region_cdc_client(1));
    let req = suite.new_changedata_request(1);
    block_on(req_tx.send((req, WriteFlags::default()))).unwrap();
    receive_event(false); // Wait until the initialization finishes.

    // Old value cache will be updated because there is 1 capture.
    suite.must_kv_prewrite(1, vec![mutation()], b"key".to_vec(), 3.into());
    suite.must_kv_commit(1, vec![b"key".to_vec()], 3.into(), 4.into());
    check_old_value_cache(&scheduler, 1);

    drop(req_tx);
    drop(event_feed);
    drop(receive_event);
    sleep_ms(200);

    // Old value cache won't be updated because there is no captures.
    suite.must_kv_prewrite(1, vec![mutation()], b"key".to_vec(), 5.into());
    suite.must_kv_commit(1, vec![b"key".to_vec()], 5.into(), 6.into());
    check_old_value_cache(&scheduler, 1);

    fail::remove("cdc_flush_old_value_metrics");
}

#[test]
fn test_cdc_rawkv_resolved_ts() {
    let mut suite = TestSuite::new(1, ApiVersion::V2);
    let cluster = &suite.cluster;

    let region = cluster.get_region(b"");
    let region_id = region.get_id();
    let leader = region.get_peers()[0].clone();
    let node_id = leader.get_id();
    let ts_provider = cluster.sim.rl().get_causal_ts_provider(node_id).unwrap();

    let env = Arc::new(Environment::new(1));
    let channel =
        ChannelBuilder::new(env).connect(&cluster.sim.rl().get_addr(leader.get_store_id()));
    let client = TikvClient::new(channel);

    let mut req = suite.new_changedata_request(region_id);
    req.set_kv_api(ChangeDataRequestKvApi::RawKv);
    let (mut req_tx, _event_feed_wrap, receive_event) =
        new_event_feed(suite.get_region_cdc_client(region_id));
    block_on(req_tx.send((req, WriteFlags::default()))).unwrap();

    let event = receive_event(false);
    event
        .events
        .into_iter()
        .for_each(|e| match e.event.unwrap() {
            Event_oneof_event::Entries(es) => {
                assert!(es.entries.len() == 1, "{:?}", es);
                let e = &es.entries[0];
                assert_eq!(e.get_type(), EventLogType::Initialized, "{:?}", es);
            }
            other => panic!("unknown event {:?}", other),
        });
    // Sleep a while to make sure the stream is registered.
    sleep_ms(1000);

    let mut ctx = Context::default();
    ctx.set_region_id(region.get_id());
    ctx.set_region_epoch(region.get_region_epoch().clone());
    ctx.set_peer(leader);
    ctx.set_api_version(ApiVersion::V2);
    let mut put_req = RawPutRequest::default();
    put_req.set_context(ctx);
    put_req.key = b"rk3".to_vec();
    put_req.value = b"v3".to_vec();

    let pause_write_fp = "raftkv_async_write";
    fail::cfg(pause_write_fp, "pause").unwrap();
    let ts = block_on(ts_provider.async_get_ts()).unwrap();
    let handle = thread::spawn(move || {
        let _ = client.raw_put(&put_req).unwrap();
    });

    sleep_ms(100);

    let event = receive_event(true).resolved_ts.unwrap();
    assert!(
        ts.next() >= TimeStamp::from(event.ts),
        "{} {}",
        ts,
        TimeStamp::from(event.ts)
    );
    // Receive again to make sure resolved ts <= ongoing request's ts.
    let event = receive_event(true).resolved_ts.unwrap();
    assert!(
        ts.next() >= TimeStamp::from(event.ts),
        "{} {}",
        ts,
        TimeStamp::from(event.ts)
    );

    fail::remove(pause_write_fp);
    handle.join().unwrap();
}

// Test one region can be subscribed multiple times in one stream with different
// `request_id`s.
#[test]
fn test_cdc_stream_multiplexing() {
    let cluster = new_server_cluster(0, 2);
    cluster.pd_client.disable_default_operator();
    let mut suite = TestSuiteBuilder::new().cluster(cluster).build();
    let rid = suite.cluster.get_region(&[]).id;
    let (mut req_tx, _, receive_event) = new_event_feed_v2(suite.get_region_cdc_client(rid));

    // Subscribe the region with request_id 1.
    let mut req = suite.new_changedata_request(rid);
    req.request_id = 1;
    block_on(req_tx.send((req, WriteFlags::default()))).unwrap();
    receive_event(false);

    // Subscribe the region with request_id 2.
    fail::cfg("before_post_incremental_scan", "pause").unwrap();
    let mut req = suite.new_changedata_request(rid);
    req.request_id = 2;
    block_on(req_tx.send((req, WriteFlags::default()))).unwrap();
    receive_event(false);

    // Request 2 can't receive a ResolvedTs, because it's not ready.
    for _ in 0..10 {
        let event = receive_event(true);
        let req_id = event.get_resolved_ts().get_request_id();
        assert_eq!(req_id, 1);
    }

    // After request 2 is ready, it must receive a ResolvedTs.
    fail::remove("before_post_incremental_scan");
    let mut request_2_ready = false;
    for _ in 0..20 {
        let event = receive_event(true);
        let req_id = event.get_resolved_ts().get_request_id();
        if req_id == 2 {
            request_2_ready = true;
            break;
        }
    }
    assert!(request_2_ready);
}

// This case tests pending regions can still get region split/merge
// notifications.
#[test]
fn test_cdc_notify_pending_regions() {
    let cluster = new_server_cluster(0, 1);
    cluster.pd_client.disable_default_operator();
    let mut suite = TestSuiteBuilder::new().cluster(cluster).build();
    let region = suite.cluster.get_region(&[]);
    let rid = region.id;
    let (mut req_tx, _, receive_event) = new_event_feed_v2(suite.get_region_cdc_client(rid));

    fail::cfg("cdc_before_initialize", "pause").unwrap();
    let mut req = suite.new_changedata_request(rid);
    req.request_id = 1;
    block_on(req_tx.send((req, WriteFlags::default()))).unwrap();

    thread::sleep(Duration::from_millis(100));
    suite.cluster.must_split(&region, b"x");
    let event = receive_event(false);
    matches!(
        event.get_events()[0].event,
        Some(Event_oneof_event::Error(ref e)) if e.has_region_not_found(),
    );
    fail::remove("cdc_before_initialize");
}

// The case check whether https://github.com/tikv/tikv/issues/17233 is fixed or not.
#[test]
fn test_delegate_fail_during_incremental_scan() {
    let mut cluster = new_server_cluster(0, 1);
    configure_for_lease_read(&mut cluster.cfg, Some(100), Some(10));
    cluster.pd_client.disable_default_operator();
    let mut suite = TestSuiteBuilder::new().cluster(cluster).build();
    let region = suite.cluster.get_region(&[]);
    let rid = region.id;
    let cf_tso = block_on(suite.cluster.pd_client.get_tso()).unwrap();

    let start_tso = cf_tso.next();
    let pk = format!("key_{:03}", 0).into_bytes();
    let mut mutations = Vec::with_capacity(10);
    for i in 0..10 {
        let mut mutation = Mutation::default();
        mutation.set_op(Op::Put);
        mutation.key = format!("key_{:03}", i).into_bytes();
        mutation.value = vec![b'x'; 16];
        mutations.push(mutation);
    }
    suite.must_kv_prewrite(rid, mutations, pk.clone(), start_tso);

    fail::cfg("before_schedule_incremental_scan", "1*pause").unwrap();

    let (mut req_tx, recv, receive_event) = new_event_feed_v2(suite.get_region_cdc_client(rid));
    let mut req = suite.new_changedata_request(rid);
    req.request_id = 100;
    req.checkpoint_ts = cf_tso.into_inner();
    req.set_start_key(Key::from_raw(b"a").into_encoded());
    req.set_end_key(Key::from_raw(b"z").into_encoded());
    block_on(req_tx.send((req.clone(), WriteFlags::default()))).unwrap();
    std::thread::sleep(Duration::from_millis(500));

    suite.cluster.must_split(&region, b"f");

    // After the incremental scan is canceled, we can get the epoch_not_match error.
    // And after the error is retrieved, no more entries can be received.
    let mut get_epoch_not_match = false;
    while !get_epoch_not_match {
        for event in receive_event(false).events.to_vec() {
            match event.event {
                Some(Event_oneof_event::Error(err)) => {
                    assert!(err.has_epoch_not_match(), "{:?}", err);
                    get_epoch_not_match = true;
                }
                Some(Event_oneof_event::Entries(..)) => {
                    assert!(!get_epoch_not_match);
                }
                _ => unreachable!(),
            }
        }
    }

    fail::remove("before_schedule_incremental_scan");

    let mut recver = recv.replace(None).unwrap();
    recv_timeout(&mut recver, Duration::from_secs(1)).unwrap_err();
    recv.replace(Some(recver));
}

// The case shows it's possible that unordered Prewrite events on one same key
// can be sent to TiCDC clients. Generally it only happens when a region changes
// during a Pipelined-DML transaction.
//
// To ensure TiCDC can handle the situation, `generation` should be carried in
// Prewrite events.
#[test]
fn test_cdc_pipeline_dml() {
    let mut cluster = new_server_cluster(0, 1);
    configure_for_lease_read(&mut cluster.cfg, Some(100), Some(10));
    cluster.pd_client.disable_default_operator();
    let mut suite = TestSuiteBuilder::new().cluster(cluster).build();
    let region = suite.cluster.get_region(&[]);
    let rid = region.id;

    let prewrite_tso = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    let (k, v) = (b"key".to_vec(), vec![b'x'; 16]);
    let mut mutation = Mutation::default();
    mutation.set_op(Op::Put);
    mutation.key = k.clone();
    mutation.value = v;
    suite.must_kv_flush(rid, vec![mutation], k.clone(), prewrite_tso, 1);

    fail::cfg("cdc_incremental_scan_start", "pause").unwrap();

    let cf_tso = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    let (mut req_tx, _, receive_event) = new_event_feed_v2(suite.get_region_cdc_client(rid));
    let mut req = suite.new_changedata_request(rid);
    req.request_id = 1;
    req.checkpoint_ts = cf_tso.into_inner();
    block_on(req_tx.send((req, WriteFlags::default()))).unwrap();
    sleep_ms(100);

    let (k, v) = (b"key".to_vec(), vec![b'y'; 16]);
    let mut mutation = Mutation::default();
    mutation.set_op(Op::Put);
    mutation.key = k.clone();
    mutation.value = v;
    suite.must_kv_flush(rid, vec![mutation], k.clone(), prewrite_tso, 2);

    let events = receive_event(false).take_events().into_vec();
    for entry in events[0].get_entries().get_entries() {
        assert_eq!(entry.r_type, EventLogType::Prewrite);
        assert_eq!(entry.generation, 2);
        assert_eq!(entry.value, vec![b'y'; 16]);
    }

    let commit_tso = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    suite.must_kv_commit(rid, vec![b"key".to_vec()], prewrite_tso, commit_tso);

    let events = receive_event(false).take_events().into_vec();
    for entry in events[0].get_entries().get_entries() {
        assert_eq!(entry.r_type, EventLogType::Commit);
        assert_eq!(entry.start_ts, prewrite_tso.into_inner());
        assert_eq!(entry.commit_ts, commit_tso.into_inner());
    }

    fail::remove("cdc_incremental_scan_start");

    let events = receive_event(false).take_events().into_vec();
    let entries = events[0].get_entries().get_entries();
    assert_eq!(entries[0].r_type, EventLogType::Prewrite);
    assert_eq!(entries[0].generation, 1);
    assert_eq!(entries[0].value, vec![b'x'; 16]);
    assert_eq!(entries[1].r_type, EventLogType::Initialized);
}

#[test]
fn test_cdc_unresolved_region_count_before_finish_scan_lock() {
    fn check_unresolved_region_count(scheduler: &Scheduler<Task>, target_count: usize) {
        let start = Instant::now();
        loop {
            sleep_ms(100);
            let (tx, rx) = mpsc::sync_channel(1);
            let checker = move |c: usize| tx.send(c).unwrap();
            scheduler
                .schedule(Task::Validate(Validate::UnresolvedRegion(Box::new(
                    checker,
                ))))
                .unwrap();
            let actual_count = rx.recv().unwrap();
            if actual_count == target_count {
                return;
            }
            if start.elapsed() > Duration::from_secs(5) {
                panic!(
                    "check unresolve region failed, actual_count: {}, target_count: {}",
                    actual_count, target_count
                );
            }
        }
    }

    let cluster = new_server_cluster(0, 1);
    let mut suite = TestSuiteBuilder::new().cluster(cluster).build();

    // create regions
    let region_count = 100;
    let split_keys: Vec<Vec<u8>> = (1..=region_count * 2 - 1)
        .step_by(2)
        .map(|i| format!("key_{:03}", i).into_bytes())
        .collect();
    let get_keys: Vec<Vec<u8>> = (0..=region_count * 2)
        .step_by(2)
        .map(|i| format!("key_{:03}", i).into_bytes())
        .collect();
    for i in 0..region_count - 1 {
        let split_key = &split_keys[i];
        let target_region = suite.cluster.get_region(split_key);
        suite.cluster.must_split(&target_region, split_key);
    }
    let mut regions = Vec::with_capacity(region_count);
    for i in 0..region_count {
        let get_key = &get_keys[i];
        let region = suite.cluster.get_region(get_key);
        regions.push(region.clone());
    }

    fail::cfg("before_schedule_resolver_ready", "pause").unwrap();

    // create event feed for all regions
    let mut req_txs = Vec::with_capacity(region_count);
    let mut event_feeds = Vec::with_capacity(region_count);
    let mut receive_events = Vec::with_capacity(region_count);
    for region in regions.clone() {
        let (mut req_tx, event_feed, receive_event) =
            new_event_feed(suite.get_region_cdc_client(region.id));
        let mut req = suite.new_changedata_request(region.id);
        req.mut_header().set_ticdc_version("7.0.0".into());
        req.set_region_epoch(region.get_region_epoch().clone());
        block_on(req_tx.send((req, WriteFlags::default()))).unwrap();
        req_txs.push(req_tx);
        event_feeds.push(event_feed);
        receive_events.push(receive_event);
    }

    check_unresolved_region_count(&suite.endpoints[&1].scheduler(), region_count);

    // Wait until all initialization finishes and check again.
    fail::remove("before_schedule_resolver_ready");
    for receive_event in receive_events {
        receive_event(false);
    }
    check_unresolved_region_count(&suite.endpoints[&1].scheduler(), 0);

    for req_tx in req_txs {
        drop(req_tx);
    }
    for event_feed in event_feeds {
        drop(event_feed);
    }
    suite.stop();
}
