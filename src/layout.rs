use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Bounds {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Bounds {
    pub fn contains(self, x: f32, y: f32) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.w && y < self.y + self.h
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum Axis {
    Horizontal,
    Vertical,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Layout {
    Pane(u64),
    Split {
        axis: Axis,
        ratio: f32,
        first: Box<Layout>,
        second: Box<Layout>,
    },
}

#[derive(Clone, Debug)]
pub struct Divider {
    pub bounds: Bounds,
    pub parent: Bounds,
    pub path: Vec<bool>,
    pub axis: Axis,
}

impl Layout {
    pub fn split(&mut self, id: u64, new_id: u64, axis: Axis) -> bool {
        match self {
            Self::Pane(old) if *old == id => {
                *self = Self::Split {
                    axis,
                    ratio: 0.5,
                    first: Box::new(Self::Pane(id)),
                    second: Box::new(Self::Pane(new_id)),
                };
                true
            }
            Self::Pane(_) => false,
            Self::Split { first, second, .. } => {
                first.split(id, new_id, axis) || second.split(id, new_id, axis)
            }
        }
    }

    pub fn ids(&self) -> Vec<u64> {
        match self {
            Self::Pane(id) => vec![*id],
            Self::Split { first, second, .. } => [first.ids(), second.ids()].concat(),
        }
    }

    pub fn remove(self, id: u64) -> Option<Self> {
        match self {
            Self::Pane(found) => (found != id).then_some(Self::Pane(found)),
            Self::Split {
                axis,
                ratio,
                first,
                second,
            } => match (first.remove(id), second.remove(id)) {
                (Some(first), Some(second)) => Some(Self::Split {
                    axis,
                    ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (Some(one), None) | (None, Some(one)) => Some(one),
                (None, None) => None,
            },
        }
    }

    pub fn set_ratio(&mut self, path: &[bool], value: f32) {
        if let Self::Split {
            ratio,
            first,
            second,
            ..
        } = self
        {
            if let Some((&side, rest)) = path.split_first() {
                if side {
                    second.set_ratio(rest, value)
                } else {
                    first.set_ratio(rest, value)
                }
            } else {
                *ratio = value.clamp(0.15, 0.85);
            }
        }
    }

    pub fn arrange(&self, bounds: Bounds) -> (Vec<(u64, Bounds)>, Vec<Divider>) {
        let mut panes = Vec::new();
        let mut dividers = Vec::new();
        self.collect(bounds, Vec::new(), &mut panes, &mut dividers);
        (panes, dividers)
    }

    fn collect(
        &self,
        bounds: Bounds,
        path: Vec<bool>,
        panes: &mut Vec<(u64, Bounds)>,
        dividers: &mut Vec<Divider>,
    ) {
        match self {
            Self::Pane(id) => panes.push((*id, bounds)),
            Self::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let mut a = bounds;
                let mut b = bounds;
                let mut line = bounds;
                match axis {
                    Axis::Horizontal => {
                        a.w = ((bounds.w - 6.0) * ratio).max(1.0);
                        b.x += a.w + 6.0;
                        b.w = (bounds.w - a.w - 6.0).max(1.0);
                        line.x += a.w;
                        line.w = 6.0;
                    }
                    Axis::Vertical => {
                        a.h = ((bounds.h - 6.0) * ratio).max(1.0);
                        b.y += a.h + 6.0;
                        b.h = (bounds.h - a.h - 6.0).max(1.0);
                        line.y += a.h;
                        line.h = 6.0;
                    }
                }
                dividers.push(Divider {
                    bounds: line,
                    parent: bounds,
                    path: path.clone(),
                    axis: *axis,
                });
                let mut left = path.clone();
                left.push(false);
                let mut right = path;
                right.push(true);
                first.collect(a, left, panes, dividers);
                second.collect(b, right, panes, dividers);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_split_removal_preserves_other_panes() {
        let mut layout = Layout::Pane(1);
        layout.split(1, 2, Axis::Horizontal);
        layout.split(2, 3, Axis::Vertical);
        let layout = layout.remove(2).unwrap();
        assert_eq!(layout.ids(), vec![1, 3]);
        let bounds = Bounds {
            x: 10.0,
            y: 20.0,
            w: 800.0,
            h: 600.0,
        };
        let (panes, dividers) = layout.arrange(bounds);
        assert_eq!(dividers.len(), 1);
        assert_eq!(panes.len(), 2);
        assert!((panes[0].1.w + panes[1].1.w + 6.0 - bounds.w).abs() < 0.01);
    }

    #[test]
    fn divider_resize_stays_inside_parent() {
        let mut layout = Layout::Pane(1);
        layout.split(1, 2, Axis::Horizontal);
        layout.set_ratio(&[], 20.0);
        let (panes, _) = layout.arrange(Bounds {
            w: 1000.0,
            h: 600.0,
            ..Default::default()
        });
        assert!(panes[0].1.w < 850.0);
        assert!(panes[1].1.w > 100.0);
    }
}
