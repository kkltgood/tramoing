//! 任务栏统计项的持久顺序。显示开关只隐藏项目，不删除它的排序位置。

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskbarItem {
    Speed,
    Performance,
}

impl TaskbarItem {
    const ALL: [Self; 2] = [Self::Speed, Self::Performance];

    fn key(self) -> &'static str {
        match self {
            Self::Speed => "speed",
            Self::Performance => "performance",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskbarOrder(Vec<TaskbarItem>);

impl Default for TaskbarOrder {
    fn default() -> Self {
        Self(TaskbarItem::ALL.to_vec())
    }
}

impl TaskbarOrder {
    /// 去重并补齐缺少的项目，后续新增统计项时追加在已有顺序之后。
    pub fn new(items: impl IntoIterator<Item = TaskbarItem>) -> Self {
        let mut order = Vec::new();
        for item in items.into_iter().chain(TaskbarItem::ALL) {
            if !order.contains(&item) {
                order.push(item);
            }
        }
        Self(order)
    }

    pub fn parse(value: &str) -> Option<Self> {
        let items: Vec<_> = value
            .split(',')
            .filter_map(|key| {
                TaskbarItem::ALL
                    .into_iter()
                    .find(|item| item.key() == key.trim())
            })
            .collect();
        (!items.is_empty()).then(|| Self::new(items))
    }

    pub fn serialize(&self) -> String {
        self.0
            .iter()
            .map(|item| item.key())
            .collect::<Vec<_>>()
            .join(",")
    }

    pub fn items(&self) -> &[TaskbarItem] {
        &self.0
    }

    /// 将拖动项插入可见项目的目标位置，其余可见项依次移动，隐藏项位置保持。
    pub fn move_visible(&mut self, item: TaskbarItem, visible: &[TaskbarItem], target: usize) {
        move_visible(&mut self.0, item, visible, target);
    }
}

fn move_visible<T: Copy + Eq>(order: &mut [T], item: T, visible: &[T], target: usize) {
    let mut reordered: Vec<_> = order
        .iter()
        .copied()
        .filter(|id| visible.contains(id))
        .collect();
    let Some(source) = reordered.iter().position(|id| *id == item) else {
        return;
    };
    let target = target.min(reordered.len() - 1);
    if source < target {
        reordered[source..=target].rotate_left(1);
    } else {
        reordered[target..=source].rotate_right(1);
    }
    let mut reordered = reordered.into_iter();
    for slot in order.iter_mut().filter(|id| visible.contains(id)) {
        *slot = reordered.next().unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_order_ignores_unknown_items_and_appends_missing_items() {
        let order = TaskbarOrder::parse(" performance,unknown,performance,speed ").unwrap();
        assert_eq!(
            order.items(),
            &[TaskbarItem::Performance, TaskbarItem::Speed]
        );
        assert_eq!(TaskbarOrder::parse("performance"), Some(order.clone()));
        assert_eq!(TaskbarOrder::parse(&order.serialize()), Some(order));
        assert_eq!(TaskbarOrder::parse("speed"), Some(TaskbarOrder::default()));
        assert_eq!(TaskbarOrder::parse("unknown,,"), None);
    }

    #[test]
    fn dragging_inserts_among_multiple_items_and_keeps_hidden_slots() {
        let original = ['a', 'b', 'c', 'd', 'e'];
        let visible = ['a', 'c', 'd', 'e'];
        let mut order = original;
        move_visible(&mut order, 'a', &visible, 2);
        assert_eq!(order, ['c', 'b', 'd', 'a', 'e']);
        move_visible(&mut order, 'e', &visible, 0);
        assert_eq!(order, ['e', 'b', 'c', 'd', 'a']);
        move_visible(&mut order, 'c', &visible, usize::MAX);
        assert_eq!(order, ['e', 'b', 'd', 'a', 'c']);
        move_visible(&mut order, 'b', &visible, 0);
        assert_eq!(order, ['e', 'b', 'd', 'a', 'c']);
        move_visible(&mut order, 'e', &[], 0);
        move_visible(&mut order, 'e', &['e'], 3);
        assert_eq!(order, ['e', 'b', 'd', 'a', 'c']);
    }
}
