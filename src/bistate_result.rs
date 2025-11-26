#[derive(Default)]
pub struct BistateResult<T, E>(pub T, pub E);

impl<CT, CE, T, E> FromIterator<Result<T, E>> for BistateResult<CT, CE>
where
    CT: FromIterator<T>,
    CE: Extend<E> + Default,
{
    fn from_iter<I: IntoIterator<Item = Result<T, E>>>(iter: I) -> Self {
        let mut b: CE = Default::default();
        let a = CT::from_iter(iter.into_iter().filter_map(|itm| match itm {
            Ok(x) => Some(x),
            Err(e) => {
                b.extend(Some(e));
                None
            }
        }));
        Self(a, b)
    }
}

impl<CT, CE, T, E> Extend<Result<T, E>> for BistateResult<CT, CE>
where
    CT: Extend<T>,
    CE: Extend<E>,
{
    fn extend<I: IntoIterator<Item = Result<T, E>>>(&mut self, iter: I) {
        iter.into_iter().for_each(|r| match r {
            Ok(t) => self.0.extend(Some(t)),
            Err(e) => self.1.extend(Some(e)),
        })
    }
}
