use super::*;

fn r(x: i32, y: i32, w: i32, h: i32) -> Rect {
    Rect { x, y, w, h }
}

fn connected(rects: &[Rect]) -> bool {
    let mut seen = vec![false; rects.len()];
    let mut stack = vec![0];
    seen[0] = true;
    while let Some(i) = stack.pop() {
        for j in 0..rects.len() {
            if !seen[j] && rects[i].touches(&rects[j]) {
                seen[j] = true;
                stack.push(j);
            }
        }
    }
    seen.iter().all(|&s| s)
}

#[test]
fn mode_indices_decode_by_the_virtual_mode_flag() {
    // plain 32-bit index
    assert_eq!(decode_mode_index(0, 3, 8), Some(3));
    assert_eq!(decode_mode_index(0, IDX32_INVALID, 8), None);
    // virtual mode: the mode index is the high half, the low half is the clone group
    assert_eq!(
        decode_mode_index(PATH_SUPPORT_VIRTUAL_MODE, (5 << 16) | 2, 8),
        Some(5)
    );
    assert_eq!(
        decode_mode_index(PATH_SUPPORT_VIRTUAL_MODE, (IDX16_INVALID << 16) | 1, 8),
        None
    );
    // out of range is never indexed
    assert_eq!(decode_mode_index(0, 9, 8), None);
}

#[test]
fn an_untouched_layout_comes_back_as_it_was() {
    let layout = [
        r(0, 0, 1920, 1080),
        r(1920, 0, 1280, 720),
        r(-1280, 200, 1280, 720),
    ];
    let out = arrange(&layout, &[false; 3], 0).unwrap();
    assert_eq!(out, layout);
}

#[test]
fn the_primary_stays_at_the_origin_when_it_is_the_one_dragged() {
    // dragging the primary right is the same as moving everything else left
    let wanted = [r(300, 0, 1920, 1080), r(1920, 0, 1280, 720)];
    let out = arrange(&wanted, &[true, false], 0).unwrap();
    assert_eq!(out[0], r(0, 0, 1920, 1080));
    assert_eq!(out[1], r(1620, 0, 1280, 720).with_snapped_to(&out[0]));
}

impl Rect {
    /// For readability in expectations: `self` moved to touch `o` the way `snap` would.
    fn with_snapped_to(self, o: &Rect) -> Rect {
        snap(self, &[*o])
    }
}

#[test]
fn a_gap_closes_to_the_nearest_edge() {
    let wanted = [r(0, 0, 1920, 1080), r(2500, 100, 1280, 720)];
    let out = arrange(&wanted, &[false, true], 0).unwrap();
    assert_eq!(out[1], r(1920, 100, 1280, 720));
    assert!(connected(&out));
}

#[test]
fn an_overlap_is_pushed_out_the_shortest_way() {
    let wanted = [r(0, 0, 1920, 1080), r(1800, 300, 1280, 720)];
    let out = arrange(&wanted, &[false, true], 0).unwrap();
    assert_eq!(out[1], r(1920, 300, 1280, 720));
    let deep = [r(0, 0, 1920, 1080), r(100, 100, 1280, 720)];
    let out = arrange(&deep, &[false, true], 0).unwrap();
    assert!(!out[0].overlaps(&out[1]));
    assert!(out[0].touches(&out[1]));
}

#[test]
fn a_display_placed_on_another_moves_around_it_and_the_rest_stay() {
    let wanted = [
        r(0, 0, 1920, 1080),
        r(1920, 0, 1280, 720),
        // dropped onto the display at 1920
        r(2000, 100, 1280, 720),
    ];
    let out = arrange(&wanted, &[false, false, true], 0).unwrap();
    assert_eq!(out[1], wanted[1]);
    for (i, a) in out.iter().enumerate() {
        for b in out.iter().skip(i + 1) {
            assert!(!a.overlaps(b), "{a:?} overlaps {b:?}");
        }
    }
    assert!(connected(&out));
}

#[test]
fn a_display_left_stranded_by_a_move_is_reattached() {
    // C hangs off B; B moves to the left of the primary, so C must find a new neighbour
    let wanted = [
        r(0, 0, 1920, 1080),
        r(-1280, 0, 1280, 720),
        r(3200, 0, 1280, 720),
    ];
    let out = arrange(&wanted, &[false, true, false], 0).unwrap();
    assert_eq!(out[1], r(-1280, 0, 1280, 720));
    assert_eq!(out[2], r(1920, 0, 1280, 720));
    assert!(connected(&out));
}

#[test]
fn corners_do_not_count_as_touching() {
    let a = r(0, 0, 100, 100);
    assert!(!a.touches(&r(100, 100, 50, 50)));
    assert!(a.touches(&r(100, 99, 50, 50)));
    assert!(a.touches(&r(-50, 0, 50, 10)));
}

#[test]
fn nothing_to_arrange_is_an_error() {
    assert!(arrange(&[], &[], 0).is_err());
    assert!(arrange(&[r(0, 0, 0, 10)], &[false], 0).is_err());
}

#[test]
fn the_watcher_reports_only_a_changed_geometry() {
    let d = |x| ArrangedDisplay {
        id: "a".into(),
        name: "A".into(),
        x,
        y: 0,
        width: 10,
        height: 10,
        primary: x == 0,
        ours: false,
        mirror_group: None,
        refresh_hz: None,
        join_id: None,
    };
    let l = |x| DisplayLayout {
        displays: vec![d(0), d(x)],
        units: "pixels".into(),
        can_apply: true,
        reason: None,
    };
    assert!(same_geometry(&l(10), &l(10)));
    assert!(!same_geometry(&l(10), &l(20)));
}

/// Every mode field except the source position, per path, so a test can prove an apply
/// changed nothing but positions.
fn mode_fingerprint() -> Vec<String> {
    let ccd = query(refresh_rate_aware()).expect("query");
    let mut out = Vec::new();
    for (i, m) in ccd.modes.iter().enumerate() {
        let line = unsafe {
            if m.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE {
                let s = m.Anonymous.sourceMode;
                format!(
                    "{i} source {}x{} fmt {}",
                    s.width, s.height, s.pixelFormat.0
                )
            } else if m.infoType
                == windows::Win32::Devices::Display::DISPLAYCONFIG_MODE_INFO_TYPE_TARGET
            {
                let t = m.Anonymous.targetMode.targetVideoSignalInfo;
                format!(
                    "{i} target {}x{} v {}/{} px {} scan {}",
                    t.activeSize.cx,
                    t.activeSize.cy,
                    t.vSyncFreq.Numerator,
                    t.vSyncFreq.Denominator,
                    t.pixelRate,
                    t.scanLineOrdering.0
                )
            } else {
                let d = m.Anonymous.desktopImageInfo;
                format!(
                    "{i} desktop {}x{} region {:?}",
                    d.PathSourceSize.x,
                    d.PathSourceSize.y,
                    (
                        d.DesktopImageRegion.left,
                        d.DesktopImageRegion.top,
                        d.DesktopImageRegion.right,
                        d.DesktopImageRegion.bottom
                    )
                )
            }
        };
        out.push(format!(
            "{:?}/{} {line}",
            (m.adapterId.LowPart, m.adapterId.HighPart),
            m.id
        ));
    }
    out.sort();
    out
}

fn print_layout(what: &str, l: &DisplayLayout) {
    println!("--- {what} (can_apply {}, {:?})", l.can_apply, l.reason);
    for d in &l.displays {
        println!(
            "  {:>6},{:<6} {}x{} primary={} ours={} mirror={:?} {:?} {}",
            d.x, d.y, d.width, d.height, d.primary, d.ours, d.mirror_group, d.name, d.id
        );
    }
}

fn gdi_rects() -> Vec<(i32, i32, u32, u32)> {
    let mut v: Vec<_> = gdi_layout()
        .into_iter()
        .map(|d| (d.x, d.y, d.width, d.height))
        .collect();
    v.sort();
    v
}

fn wait_for(count: usize) -> DisplayLayout {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        let l = read_layout().expect("read");
        if l.displays.len() == count || std::time::Instant::now() > deadline {
            return l;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Creates two virtual displays, moves only them through `apply` (beside, left of and above the
/// primary; then an overlapping and a gapped request), checks every step by reading back and
/// through GDI, and proves nothing else moved and no mode changed. Removes both at the end and
/// checks the desktop is as it started.
#[test]
#[ignore = "requires the virtual display driver installed and no ScreenExtend host running; creates two real displays and moves them; run with --ignored --nocapture"]
fn live_rearranges_only_virtual_displays() {
    let Ok(driver) =
        crate::windows_utils::virtual_display::WindowsVirtualDisplay::new_shared_reported()
    else {
        crate::test_support::skip_because(
            "display arrangement",
            "the virtual display driver is not installed",
        );
        return;
    };
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::<DisplayLayout>::new()));
    let sink = events.clone();
    let watcher = watch(move |l| sink.lock().unwrap().push(l)).expect("watcher");
    let settle = || std::thread::sleep(Duration::from_millis(1500));
    let start = read_layout().expect("read before");
    let start_fp = mode_fingerprint();
    let start_gdi = gdi_rects();
    print_layout("before anything", &start);
    println!("gdi before: {start_gdi:?}");

    let a = driver
        .create_display("ScreenExtend - arrange A".into(), 1280, 720, 60)
        .expect("create A");
    let b = driver
        .create_display("ScreenExtend - arrange B".into(), 1280, 720, 60)
        .expect("create B");
    let created = wait_for(start.displays.len() + 2);
    settle();
    let seen_on_create = events.lock().unwrap().len();
    println!("watcher events after creating: {seen_on_create}");
    assert!(
        seen_on_create >= 1,
        "the watcher missed two displays arriving"
    );
    print_layout("after creating two displays", &created);
    let theirs: Vec<&ArrangedDisplay> = created
        .displays
        .iter()
        .filter(|d| !start.displays.iter().any(|s| s.id == d.id))
        .collect();
    assert_eq!(theirs.len(), 2, "two new displays");
    let (va, vb) = (theirs[0].id.clone(), theirs[1].id.clone());
    let primary = created
        .displays
        .iter()
        .find(|d| d.primary)
        .expect("a primary")
        .clone();
    let fixed: Vec<ArrangedDisplay> = created
        .displays
        .iter()
        .filter(|d| d.id != va && d.id != vb)
        .cloned()
        .collect();
    let fp = mode_fingerprint();

    let check = |what: &str, got: &DisplayLayout| {
        print_layout(what, got);
        for f in &fixed {
            let now = got
                .displays
                .iter()
                .find(|d| d.id == f.id)
                .expect("still there");
            assert_eq!(
                (now.x, now.y, now.width, now.height),
                (f.x, f.y, f.width, f.height),
                "{what}: {} moved",
                f.name
            );
        }
        assert_eq!(mode_fingerprint(), fp, "{what}: a mode changed");
        let mut want: Vec<_> = got
            .displays
            .iter()
            .map(|d| (d.x, d.y, d.width, d.height))
            .collect();
        want.sort();
        assert_eq!(gdi_rects(), want, "{what}: GDI disagrees");
        let rects: Vec<Rect> = got
            .displays
            .iter()
            .map(|d| r(d.x, d.y, d.width as i32, d.height as i32))
            .collect();
        for (i, x) in rects.iter().enumerate() {
            for y in rects.iter().skip(i + 1) {
                assert!(!x.overlaps(y), "{what}: overlap");
            }
        }
    };
    let at = |l: &DisplayLayout, id: &str| {
        let d = l.displays.iter().find(|d| d.id == id).expect("display");
        (d.x, d.y)
    };
    let pw = primary.width as i32;
    let ph = primary.height as i32;

    let steps: Vec<(&str, Vec<DisplayPlacement>)> = vec![
        (
            "A right of the primary, B left of it",
            vec![
                DisplayPlacement {
                    id: va.clone(),
                    x: pw,
                    y: 0,
                },
                DisplayPlacement {
                    id: vb.clone(),
                    x: -1280,
                    y: 0,
                },
            ],
        ),
        (
            "A above the primary",
            vec![DisplayPlacement {
                id: va.clone(),
                x: 0,
                y: -720,
            }],
        ),
        (
            "B dropped onto the primary (overlap)",
            vec![DisplayPlacement {
                id: vb.clone(),
                x: 100,
                y: 100,
            }],
        ),
        (
            "B far to the right (gap)",
            vec![DisplayPlacement {
                id: vb.clone(),
                x: pw + 900,
                y: 50,
            }],
        ),
    ];
    for (what, placements) in steps {
        let got = WindowsArranger::apply(&placements).expect(what);
        check(what, &got);
        let again = read_layout().expect("re-read");
        assert_eq!(
            again, got,
            "{what}: a fresh read differs from what apply returned"
        );
        println!(
            "  {what}: A at {:?}, B at {:?}",
            at(&got, &va),
            at(&got, &vb)
        );
    }
    settle();
    let after_applies = events.lock().unwrap().len();
    println!(
        "watcher events after our four applies: {}",
        after_applies - seen_on_create
    );
    assert_eq!(
        after_applies, seen_on_create,
        "our own applies were reported back"
    );
    let final_layout = read_layout().unwrap();
    // the exact spots the requests should have produced, where no other display was in the way
    assert!(
        at(&final_layout, &vb).0 >= pw,
        "B should end up to the right"
    );
    let _ = ph;

    driver.remove_display(a);
    driver.remove_display(b);
    let end = wait_for(start.displays.len());
    print_layout("after removing both", &end);
    settle();
    let after_remove = events.lock().unwrap().len();
    println!(
        "watcher events after removing: {}",
        after_remove - after_applies
    );
    assert!(
        after_remove > after_applies,
        "the watcher missed two displays leaving"
    );
    drop(watcher);
    let mut s: Vec<_> = start
        .displays
        .iter()
        .map(|d| (d.id.clone(), d.x, d.y, d.width, d.height))
        .collect();
    let mut e: Vec<_> = end
        .displays
        .iter()
        .map(|d| (d.id.clone(), d.x, d.y, d.width, d.height))
        .collect();
    s.sort();
    e.sort();
    assert_eq!(e, s, "the desktop is not as it started");
    assert_eq!(
        gdi_rects(),
        start_gdi,
        "GDI: the desktop is not as it started"
    );
    let end_fp = mode_fingerprint();
    if end_fp != start_fp {
        println!("NOTE: modes differ from the start after the displays left:\n  before {start_fp:?}\n  after  {end_fp:?}");
    }
}
