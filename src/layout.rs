/// Window coordinates and dimensions in compositor logical pixels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LogicalRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl LogicalRect {
    pub fn is_valid(self) -> bool {
        self.width > 0 && self.height > 0
    }

    pub fn overlaps(self, other: Self) -> bool {
        let (left, right, top, bottom) = self.edges();
        let (other_left, other_right, other_top, other_bottom) = other.edges();
        left < other_right && other_left < right && top < other_bottom && other_top < bottom
    }

    pub fn center(self) -> Option<(i32, i32)> {
        let x = i64::from(self.x) + i64::from(self.width / 2);
        let y = i64::from(self.y) + i64::from(self.height / 2);
        Some((i32::try_from(x).ok()?, i32::try_from(y).ok()?))
    }

    pub fn scroll_distance(self) -> f64 {
        (f64::from(self.height) - 160.0).max(240.0)
    }

    fn edges(self) -> (i64, i64, i64, i64) {
        let x = i64::from(self.x);
        let y = i64::from(self.y);
        (x, x + i64::from(self.width), y, y + i64::from(self.height))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GridSpec {
    pub columns: u32,
    pub rows: u32,
    count: u32,
}

impl GridSpec {
    pub fn for_count(count: usize) -> Result<Self, String> {
        if !(1..=16).contains(&count) {
            return Err("layout requires between 1 and 16 windows".into());
        }
        let count = count as u32;
        let columns = (count as f64).sqrt().ceil() as u32;
        let rows = count.div_ceil(columns);
        Ok(Self {
            columns,
            rows,
            count,
        })
    }

    pub fn cells(self, work_area: LogicalRect) -> Result<Vec<LogicalRect>, String> {
        if !work_area.is_valid() {
            return Err("layout work area is invalid".into());
        }
        (0..self.count)
            .map(|index| {
                let column = index % self.columns;
                let row = index / self.columns;
                let x_start = partition(work_area.width, column, self.columns);
                let x_end = partition(work_area.width, column + 1, self.columns);
                let y_start = partition(work_area.height, row, self.rows);
                let y_end = partition(work_area.height, row + 1, self.rows);
                Ok(LogicalRect {
                    x: add_offset(work_area.x, x_start)?,
                    y: add_offset(work_area.y, y_start)?,
                    width: x_end - x_start,
                    height: y_end - y_start,
                })
            })
            .collect()
    }
}

pub fn has_overlaps(rectangles: &[LogicalRect]) -> bool {
    rectangles.iter().enumerate().any(|(index, rectangle)| {
        rectangles[index + 1..]
            .iter()
            .any(|other| rectangle.overlaps(*other))
    })
}

fn partition(dimension: u32, boundary: u32, divisions: u32) -> u32 {
    ((u64::from(boundary) * u64::from(dimension)) / u64::from(divisions)) as u32
}

fn add_offset(origin: i32, offset: u32) -> Result<i32, String> {
    i32::try_from(i64::from(origin) + i64::from(offset))
        .map_err(|_| "layout coordinate exceeds supported logical-pixel range".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_partitions_odd_display_dimensions_without_gaps() {
        let area = LogicalRect {
            x: -1366,
            y: 28,
            width: 2731,
            height: 1517,
        };
        for count in 1..=16 {
            let grid = GridSpec::for_count(count).unwrap();
            let cells = grid.cells(area).unwrap();
            assert_eq!(cells.len(), count);
            assert!(cells.iter().all(|cell| cell.is_valid()));
            assert!(!has_overlaps(&cells));
            assert_eq!(cells.first().unwrap().x, area.x);
            assert_eq!(cells.first().unwrap().y, area.y);
            for row in 0..grid.rows {
                let row_count = (count as u32 - row * grid.columns).min(grid.columns);
                let row_cells = cells
                    .iter()
                    .skip((row * grid.columns) as usize)
                    .take(row_count as usize);
                assert_eq!(
                    row_cells.map(|cell| cell.width).sum::<u32>(),
                    partition(area.width, row_count, grid.columns)
                );
            }
            assert_eq!(
                cells.last().unwrap().y + cells.last().unwrap().height as i32,
                area.y + area.height as i32
            );
        }
        assert_eq!(
            GridSpec::for_count(4).unwrap(),
            GridSpec {
                columns: 2,
                rows: 2,
                count: 4
            }
        );
        assert_eq!(
            GridSpec::for_count(5).unwrap(),
            GridSpec {
                columns: 3,
                rows: 2,
                count: 5
            }
        );
        assert!(GridSpec::for_count(0).is_err());
        assert!(GridSpec::for_count(17).is_err());
    }

    #[test]
    fn overlap_handles_touching_zero_and_extreme_coordinates() {
        let rect = LogicalRect {
            x: -10,
            y: 4,
            width: 10,
            height: 20,
        };
        assert!(!rect.overlaps(LogicalRect {
            x: 0,
            y: 4,
            width: 3,
            height: 20
        }));
        assert!(rect.overlaps(LogicalRect {
            x: -1,
            y: 5,
            width: 3,
            height: 3
        }));
        assert!(!LogicalRect {
            x: 0,
            y: 0,
            width: 0,
            height: 10
        }
        .overlaps(LogicalRect {
            x: 0,
            y: 0,
            width: 10,
            height: 10
        }));
        assert!(LogicalRect {
            x: i32::MAX,
            y: i32::MAX,
            width: u32::MAX,
            height: u32::MAX
        }
        .overlaps(LogicalRect {
            x: i32::MAX,
            y: i32::MAX,
            width: 1,
            height: 1
        }));
        assert!(!has_overlaps(&[
            rect,
            LogicalRect {
                x: 0,
                y: 4,
                width: 3,
                height: 20
            }
        ]));
    }

    #[test]
    fn grid_cells_match_the_original_floor_partition_at_edge_sizes() {
        let original_partition = |dimension: u32, boundary: u32, divisions: u32| {
            ((u64::from(boundary) * u64::from(dimension)) / u64::from(divisions)) as u32
        };
        for (width, height) in [(1, 1), (2731, 1517), (2560, 1440)] {
            let area = LogicalRect {
                x: -1920,
                y: 37,
                width,
                height,
            };
            for count in 1..=16 {
                let grid = GridSpec::for_count(count).unwrap();
                let cells = grid.cells(area).unwrap();
                for (index, cell) in cells.into_iter().enumerate() {
                    let column = index as u32 % grid.columns;
                    let row = index as u32 / grid.columns;
                    let x_start = original_partition(width, column, grid.columns);
                    let x_end = original_partition(width, column + 1, grid.columns);
                    let y_start = original_partition(height, row, grid.rows);
                    let y_end = original_partition(height, row + 1, grid.rows);
                    assert_eq!(cell.x, area.x + x_start as i32);
                    assert_eq!(cell.y, area.y + y_start as i32);
                    assert_eq!(cell.width, x_end - x_start);
                    assert_eq!(cell.height, y_end - y_start);
                }
            }
        }
    }

    #[test]
    fn geometry_centers_and_viewport_scroll_use_logical_pixels() {
        let geometry = LogicalRect {
            x: 100,
            y: 200,
            width: 800,
            height: 1000,
        };
        assert_eq!(geometry.center(), Some((500, 700)));
        assert_eq!(geometry.scroll_distance(), 840.0);
        assert_eq!(
            LogicalRect {
                height: 200,
                ..geometry
            }
            .scroll_distance(),
            240.0
        );
        assert_eq!(
            LogicalRect {
                x: i32::MAX,
                width: u32::MAX,
                ..geometry
            }
            .center(),
            None
        );
    }
}
