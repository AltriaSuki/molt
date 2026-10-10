use ringbuf::MovingAverage;

#[test]
fn empty_average_is_none() {
    let avg = MovingAverage::new(3);
    assert_eq!(avg.average(), None);
    assert_eq!(avg.delta(), None);
    assert_eq!(avg.min(), None);
    assert!(avg.is_empty());
    assert!(!avg.is_warm());
    assert_eq!(avg.window_size(), 3);
}

#[test]
#[should_panic]
fn zero_window_panics() {
    let _ = MovingAverage::new(0);
}

#[test]
fn averages_a_partial_window() {
    let mut avg = MovingAverage::new(4);
    assert_eq!(avg.add(2.0), 2.0);
    assert_eq!(avg.add(4.0), 3.0);
    assert_eq!(avg.average(), Some(3.0));
    assert_eq!(avg.len(), 2);
    assert!(!avg.is_warm());
}

#[test]
fn window_slides_once_full() {
    let mut avg = MovingAverage::new(3);
    let got: Vec<f64> = [1.0, 2.0, 3.0, 4.0, 8.0].iter().map(|&s| avg.add(s)).collect();
    assert_eq!(got, [1.0, 1.5, 2.0, 3.0, 5.0]);
    assert!(avg.is_warm());
    assert_eq!(avg.len(), 3);
}

#[test]
fn min_and_max_cover_the_window() {
    let mut avg = MovingAverage::new(3);
    for s in [5.0, -1.0, 7.0, 2.0, 3.0] {
        avg.add(s);
    }
    assert_eq!(avg.min(), Some(2.0));
    assert_eq!(avg.max(), Some(7.0));
}

#[test]
fn samples_and_delta() {
    let mut avg = MovingAverage::new(4);
    avg.add(1.0);
    avg.add(4.0);
    avg.add(2.5);
    assert_eq!(avg.samples().collect::<Vec<_>>(), [1.0, 4.0, 2.5]);
    assert_eq!(avg.delta(), Some(1.5));
}

#[test]
fn reset_forgets_the_samples() {
    let mut avg = MovingAverage::new(2);
    avg.add(10.0);
    avg.add(20.0);
    avg.add(30.0);
    avg.reset();
    assert!(avg.is_empty());
    assert_eq!(avg.average(), None);
    assert_eq!(avg.window_size(), 2);
    assert_eq!(avg.add(6.0), 6.0);
    assert_eq!(avg.samples().collect::<Vec<_>>(), [6.0]);
}
