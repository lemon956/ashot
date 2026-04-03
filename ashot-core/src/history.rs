use crate::document::Annotation;

#[derive(Debug, Clone)]
pub struct EditorHistory {
    limit: usize,
    past: Vec<Vec<Annotation>>,
    future: Vec<Vec<Annotation>>,
}

impl EditorHistory {
    pub fn new(limit: usize) -> Self {
        Self { limit: limit.max(1), past: Vec::new(), future: Vec::new() }
    }

    pub fn snapshot(&mut self, annotations: &[Annotation]) {
        self.past.push(annotations.to_vec());
        if self.past.len() > self.limit {
            self.past.remove(0);
        }
        self.future.clear();
    }

    pub fn undo(&mut self, current: &[Annotation]) -> Option<Vec<Annotation>> {
        let previous = self.past.pop()?;
        self.future.push(current.to_vec());
        Some(previous)
    }

    pub fn redo(&mut self, current: &[Annotation]) -> Option<Vec<Annotation>> {
        let next = self.future.pop()?;
        self.past.push(current.to_vec());
        Some(next)
    }
}

#[cfg(test)]
mod tests {
    use crate::document::{Annotation, AnnotationData, Color, Rect};

    use super::EditorHistory;

    #[test]
    fn supports_undo_redo() {
        let a = Annotation::new(AnnotationData::Rectangle {
            rect: Rect { x: 1.0, y: 1.0, width: 5.0, height: 5.0 },
            color: Color::rgba(255, 0, 0, 255),
            stroke_width: 2,
        });
        let b = Annotation::new(AnnotationData::Rectangle {
            rect: Rect { x: 2.0, y: 2.0, width: 5.0, height: 5.0 },
            color: Color::rgba(0, 255, 0, 255),
            stroke_width: 2,
        });

        let mut history = EditorHistory::new(8);
        history.snapshot(std::slice::from_ref(&a));

        let undone = history.undo(&[a.clone(), b.clone()]).expect("undo");
        assert_eq!(undone, vec![a.clone()]);

        let redone = history.redo(&undone).expect("redo");
        assert_eq!(redone, vec![a, b]);
    }
}
