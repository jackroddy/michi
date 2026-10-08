//! Data references checked and ranges turned into their values, with the
//! tree otherwise left as written.

use std::collections::{HashMap, HashSet};

use crate::ast::*;
use crate::error::{Error, Span};

/// The largest list a range may produce.
const MOST_VALUES: usize = 1_000_000;

/// Evaluate every range and check every reference. On success the tree
/// holds no ranges, and every data path, parameter list and sweep
/// variable in an attribute names something that exists.
pub fn resolve(file: &mut File) -> Result<(), Error> {
    for item in &mut file.items {
        match item {
            Item::Data(data) => eval_data(data)?,
            Item::Pipeline(pipeline) => eval_pipeline(pipeline)?,
        }
    }
    check(file)
}

// ---

fn eval_data(data: &mut Data) -> Result<(), Error> {
    for entry in &mut data.entries {
        check_literal(&entry.value, "a data value")?;
        eval_value(&mut entry.value)?;
        check_shape(&entry.value)?;
    }
    Ok(())
}

/// Numbers, strings, identifiers, and lists and tuples of them: nothing
/// that refers elsewhere.
fn check_literal(value: &Value, what: &str) -> Result<(), Error> {
    match &value.kind {
        ValueKind::Var(_) | ValueKind::Path(..) | ValueKind::Call(..) => Err(Error::new(
            format!("{what} is a number, a string, an identifier, a list or a tuple"),
            value.span,
        )),
        ValueKind::List(elems) => {
            for elem in elems {
                match elem {
                    Elem::Value(v) => check_literal(v, what)?,
                    Elem::Range(range) => {
                        for v in [&range.start, &range.end] {
                            check_literal(v, what)?;
                        }
                        if let Some(RangeStep::Add(v) | RangeStep::Mul(v)) = &range.step {
                            check_literal(v, what)?;
                        }
                    }
                }
            }
            Ok(())
        }
        ValueKind::Tuple(values) => values.iter().try_for_each(|v| check_literal(v, what)),
        _ => Ok(()),
    }
}

/// A list holds tuples of one width, or no tuples at all.
fn check_shape(value: &Value) -> Result<(), Error> {
    let ValueKind::List(elems) = &value.kind else {
        return Ok(());
    };

    // the first element's width, once seen: `None` for a plain value
    let mut first: Option<Option<usize>> = None;
    for elem in elems {
        let Elem::Value(v) = elem else { continue };
        let width = match &v.kind {
            ValueKind::Tuple(items) => Some(items.len()),
            _ => None,
        };
        match first {
            None => first = Some(width),
            Some(seen) if seen == width => {}
            Some(Some(seen)) if width.is_some() => {
                return Err(Error::new(
                    format!(
                        "this tuple holds {} values where the list's first holds {seen}",
                        width.unwrap_or(0)
                    ),
                    v.span,
                ));
            }
            Some(_) => {
                return Err(Error::new(
                    "a list holds tuples or plain values, not both",
                    v.span,
                ));
            }
        }
    }
    Ok(())
}

fn eval_pipeline(pipeline: &mut Pipeline) -> Result<(), Error> {
    eval_attrs(&mut pipeline.attrs)?;
    eval_pipeline_items(&mut pipeline.items)
}

fn eval_pipeline_items(items: &mut [PipelineItem]) -> Result<(), Error> {
    for item in items {
        match item {
            PipelineItem::Data(data) => eval_data(data)?,
            PipelineItem::Step(step) => eval_step(step)?,
            PipelineItem::Sweep(sweep) => {
                eval_attrs(&mut sweep.attrs)?;
                eval_params(&mut sweep.params)?;
                eval_pipeline_items(&mut sweep.body)?;
            }
        }
    }
    Ok(())
}

fn eval_step(step: &mut Step) -> Result<(), Error> {
    eval_attrs(&mut step.attrs)?;
    if let Some(params) = &mut step.params {
        eval_params(params)?;
    }
    eval_step_items(&mut step.body)
}

fn eval_step_items(items: &mut [StepItem]) -> Result<(), Error> {
    for item in items {
        match item {
            StepItem::Command(command) => eval_attrs(&mut command.attrs)?,
            StepItem::Sweep(sweep) => {
                eval_attrs(&mut sweep.attrs)?;
                eval_params(&mut sweep.params)?;
                eval_step_items(&mut sweep.body)?;
            }
        }
    }
    Ok(())
}

fn eval_attrs(attrs: &mut [Attr]) -> Result<(), Error> {
    for attr in attrs {
        for arg in &mut attr.args {
            eval_value(&mut arg.value)?;
        }
    }
    Ok(())
}

fn eval_params(params: &mut Params) -> Result<(), Error> {
    for binding in &mut params.bindings {
        if !matches!(binding.list.kind, ValueKind::Path(..)) {
            check_literal(&binding.list, "a parameter's list")?;
        }
        eval_value(&mut binding.list)?;
        check_shape(&binding.list)?;
    }
    Ok(())
}

fn eval_value(value: &mut Value) -> Result<(), Error> {
    match &mut value.kind {
        ValueKind::List(elems) => {
            let mut out = Vec::with_capacity(elems.len());
            for elem in elems.drain(..) {
                match elem {
                    Elem::Value(mut v) => {
                        eval_value(&mut v)?;
                        out.push(Elem::Value(v));
                    }
                    Elem::Range(range) => {
                        for v in expand_range(&range)? {
                            out.push(Elem::Value(v));
                        }
                    }
                }
            }
            *elems = out;
        }
        ValueKind::Tuple(values) => {
            for v in values {
                eval_value(v)?;
            }
        }
        ValueKind::Call(_, args) => {
            for arg in args {
                eval_value(&mut arg.value)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// The values a range stands for, as `Int` or `Float` elements.
fn expand_range(range: &Range) -> Result<Vec<Value>, Error> {
    let span = range.span;
    let too_long = || Error::new("range has too many values", span);
    match (&range.start.kind, &range.end.kind) {
        (ValueKind::Int(start), ValueKind::Int(end)) => {
            let step = match &range.step {
                None => RangeStep::Add(Value {
                    kind: ValueKind::Int(1),
                    span,
                }),
                Some(step) => match step {
                    RangeStep::Add(v) | RangeStep::Mul(v)
                        if !matches!(v.kind, ValueKind::Int(_)) =>
                    {
                        return Err(Error::new("an integer range takes an integer step", v.span));
                    }
                    RangeStep::Add(v) => RangeStep::Add(Value {
                        kind: ValueKind::Int(int_of(v)),
                        span: v.span,
                    }),
                    RangeStep::Mul(v) => RangeStep::Mul(Value {
                        kind: ValueKind::Int(int_of(v)),
                        span: v.span,
                    }),
                },
            };
            let mut out: Vec<Value> = Vec::new();
            let int = |n: i64| Value {
                kind: ValueKind::Int(n),
                span,
            };
            let (start, end) = (*start, *end);
            let within = |n: i64| {
                if range.inclusive { n <= end } else { n < end }
            };
            match step {
                RangeStep::Add(v) => {
                    let by = int_of(&v);
                    if by <= 0 {
                        return Err(Error::new("a range step must be positive", v.span));
                    }
                    if start > end {
                        return Err(Error::new("a range must not run backwards", span));
                    }
                    let mut n = start;
                    while within(n) {
                        out.push(int(n));
                        if out.len() > MOST_VALUES {
                            return Err(too_long());
                        }
                        n = match n.checked_add(by) {
                            Some(n) => n,
                            None => break,
                        };
                    }
                }
                RangeStep::Mul(v) => {
                    let by = int_of(&v);
                    if by <= 1 {
                        return Err(Error::new(
                            "a multiplying step must be greater than one",
                            v.span,
                        ));
                    }
                    if start <= 0 {
                        return Err(Error::new(
                            "a multiplying range starts above zero",
                            range.start.span,
                        ));
                    }
                    if start > end {
                        return Err(Error::new("a range must not run backwards", span));
                    }
                    let mut n = start;
                    while within(n) {
                        out.push(int(n));
                        n = match n.checked_mul(by) {
                            Some(n) => n,
                            None => break,
                        };
                    }
                }
            }
            Ok(out)
        }
        (ValueKind::Float { .. }, ValueKind::Float { .. }) => {
            let (start, start_text) = float_of(&range.start);
            let (end, _) = float_of(&range.end);
            let Some(step) = &range.step else {
                return Err(Error::new("a float range needs a step", span));
            };
            let (by, by_text, mul) = match step {
                RangeStep::Add(v) | RangeStep::Mul(v)
                    if !matches!(v.kind, ValueKind::Float { .. }) =>
                {
                    return Err(Error::new("a float range takes a float step", v.span));
                }
                RangeStep::Add(v) => {
                    let (by, text) = float_of(v);
                    (by, text, false)
                }
                RangeStep::Mul(v) => {
                    let (by, text) = float_of(v);
                    (by, text, true)
                }
            };
            if mul && (by <= 1.0 || start <= 0.0) {
                return Err(Error::new(
                    "a multiplying float range starts above zero and steps above one",
                    span,
                ));
            }
            if !mul && by <= 0.0 {
                return Err(Error::new("a range step must be positive", span));
            }
            if start > end {
                return Err(Error::new("a range must not run backwards", span));
            }

            // printed with the decimal places of the step or the
            // start, whichever has more, so 0.1 steps never show
            // the drift of repeated addition
            let places = decimals(start_text).max(decimals(by_text));
            let mut out = Vec::new();
            let mut i = 0u32;
            loop {
                let value = if mul {
                    start * by.powi(i as i32)
                } else {
                    start + by * f64::from(i)
                };

                // the end is reached by count, with a little room
                // for the arithmetic
                let slack = 1e-9 * (1.0 + end.abs());
                let within = if range.inclusive {
                    value <= end + slack
                } else {
                    value < end - slack
                };
                if !within {
                    break;
                }
                // a product has no fixed number of places, so it
                // prints the shortest text that reads back as itself
                let text = if mul {
                    let text = format!("{value}");
                    if text.contains('.') {
                        text
                    } else {
                        format!("{text}.0")
                    }
                } else {
                    format!("{value:.places$}")
                };
                out.push(Value {
                    kind: ValueKind::Float { value, text },
                    span,
                });
                if out.len() > MOST_VALUES {
                    return Err(too_long());
                }
                i += 1;
            }
            Ok(out)
        }
        _ => Err(Error::new(
            "a range runs between two integers or two floats",
            span,
        )),
    }
}

fn int_of(value: &Value) -> i64 {
    match value.kind {
        ValueKind::Int(n) => n,
        _ => 0,
    }
}

fn float_of(value: &Value) -> (f64, &str) {
    match &value.kind {
        ValueKind::Float { value, text } => (*value, text),
        _ => (0.0, ""),
    }
}

/// How many digits follow the point in a float as written.
fn decimals(text: &str) -> usize {
    text.find('.').map_or(0, |i| text.len() - i - 1)
}

// ---

/// The data blocks visible from inside one pipeline: the file's, then
/// the pipeline's own.
pub struct Scope<'a> {
    blocks: Vec<&'a Data>,
}

impl<'a> Scope<'a> {
    pub fn new(file: &'a File, pipeline: &'a Pipeline) -> Scope<'a> {
        let mut blocks: Vec<&Data> = file
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Data(data) => Some(data),
                _ => None,
            })
            .collect();
        blocks.extend(pipeline.items.iter().filter_map(|item| match item {
            PipelineItem::Data(data) => Some(data),
            _ => None,
        }));
        Scope { blocks }
    }

    pub fn block(&self, name: &str) -> Option<&'a Data> {
        self.blocks.iter().copied().find(|d| d.name.text == name)
    }

    pub fn lookup(&self, block: &str, key: &str) -> Option<&'a Value> {
        self.block(block)?
            .entries
            .iter()
            .find(|e| e.key.text == key)
            .map(|e| &e.value)
    }

    /// The value a path names, or the error that it names nothing.
    fn get(&self, block: &Name, key: &Name) -> Result<&'a Value, Error> {
        if self.block(&block.text).is_none() {
            return Err(Error::new(
                format!("no data block named `{}`", block.text),
                block.span,
            ));
        }
        self.lookup(&block.text, &key.text).ok_or_else(|| {
            Error::new(
                format!("data block `{}` has no `{}`", block.text, key.text),
                key.span,
            )
        })
    }
}

fn check(file: &File) -> Result<(), Error> {
    let pipelines: Vec<&Pipeline> = file
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Pipeline(p) => Some(p),
            _ => None,
        })
        .collect();
    if pipelines.is_empty() {
        return Err(Error::new("file has no pipeline", Span::new(0, 0)));
    }
    let top: Vec<&Data> = file
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Data(d) => Some(d),
            _ => None,
        })
        .collect();
    check_blocks(&top, &[])?;
    for pipeline in pipelines {
        let own: Vec<&Data> = pipeline
            .items
            .iter()
            .filter_map(|item| match item {
                PipelineItem::Data(d) => Some(d),
                _ => None,
            })
            .collect();
        check_blocks(&own, &top)?;
        let scope = Scope::new(file, pipeline);
        let mut bound = Vec::new();
        check_attrs(&pipeline.attrs, &scope, &bound)?;
        check_pipeline_items(&pipeline.items, &scope, &mut bound)?;
    }
    Ok(())
}

/// No block named twice, here or against `outer`, and no key named twice
/// in a block.
fn check_blocks(blocks: &[&Data], outer: &[&Data]) -> Result<(), Error> {
    let mut seen: Vec<&Data> = outer.to_vec();
    for block in blocks {
        check_reserved(&block.name)?;
        if let Some(earlier) = seen.iter().find(|d| d.name.text == block.name.text) {
            return Err(Error::new(
                format!(
                    "data block `{}` is already defined at offset {}",
                    block.name.text, earlier.name.span.start
                ),
                block.name.span,
            ));
        }
        seen.push(block);
        let mut keys = HashSet::new();
        for entry in &block.entries {
            check_reserved(&entry.key)?;
            if !keys.insert(entry.key.text.as_str()) {
                return Err(Error::new(
                    format!(
                        "`{}` is already defined in data block `{}`",
                        entry.key.text, block.name.text
                    ),
                    entry.key.span,
                ));
            }
        }
    }
    check_script_names(&seen)
}

/// The script flattens `block.KEY` to `block_KEY`, and a tuple list to
/// `block_KEY_1` and up, so two different names can land on one shell
/// variable.
fn check_script_names(blocks: &[&Data]) -> Result<(), Error> {
    let mut taken: HashMap<String, String> = HashMap::new();
    for block in blocks {
        for entry in &block.entries {
            let path = format!("{}.{}", block.name.text, entry.key.text);
            let flat = format!("{}_{}", block.name.text, entry.key.text);
            let mut names = vec![flat.clone()];
            if let ValueKind::List(elems) = &entry.value.kind
                && let Some(Elem::Value(first)) = elems.first()
                && let ValueKind::Tuple(items) = &first.kind
            {
                names.extend((1..=items.len()).map(|i| format!("{flat}_{i}")));
            }
            for name in names {
                if let Some(other) = taken.insert(name.clone(), path.clone()) {
                    return Err(Error::new(
                        format!("`{path}` and `{other}` both become `{name}` in the script"),
                        entry.key.span,
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Names starting with `_` are the script's own.
fn check_reserved(name: &Name) -> Result<(), Error> {
    if name.text.starts_with('_') {
        return Err(Error::new(
            format!(
                "`{}` starts with `_`, which is reserved for the script",
                name.text
            ),
            name.span,
        ));
    }
    Ok(())
}

fn check_pipeline_items(
    items: &[PipelineItem],
    scope: &Scope<'_>,
    bound: &mut Vec<String>,
) -> Result<(), Error> {
    for item in items {
        match item {
            PipelineItem::Data(_) => {}
            PipelineItem::Step(step) => check_step(step, scope, bound)?,
            PipelineItem::Sweep(sweep) => {
                let names = check_params(&sweep.params, scope, bound)?;
                let depth = bound.len();
                bound.extend(names);

                // after binding, since the attributes land on the
                // items inside, where the variables are set
                check_attrs(&sweep.attrs, scope, bound)?;
                check_pipeline_items(&sweep.body, scope, bound)?;
                bound.truncate(depth);
            }
        }
    }
    Ok(())
}

fn check_step(step: &Step, scope: &Scope<'_>, bound: &mut Vec<String>) -> Result<(), Error> {
    check_attrs(&step.attrs, scope, bound)?;
    let depth = bound.len();
    if let Some(params) = &step.params {
        let names = check_params(params, scope, bound)?;
        bound.extend(names);
    }
    check_step_items(&step.body, scope, bound)?;
    bound.truncate(depth);
    Ok(())
}

fn check_step_items(
    items: &[StepItem],
    scope: &Scope<'_>,
    bound: &mut Vec<String>,
) -> Result<(), Error> {
    for item in items {
        match item {
            StepItem::Command(command) => {
                check_attrs(&command.attrs, scope, bound)?;
                check_text(&command.text, scope, command.span)?;
            }
            StepItem::Sweep(sweep) => {
                let names = check_params(&sweep.params, scope, bound)?;
                let depth = bound.len();
                bound.extend(names);
                check_attrs(&sweep.attrs, scope, bound)?;
                check_step_items(&sweep.body, scope, bound)?;
                bound.truncate(depth);
            }
        }
    }
    Ok(())
}

/// A `${block.KEY}` in text names a scalar that exists. Any other `$x`
/// is the shell's or a sweep's and is not checked here.
fn check_text(text: &Text, scope: &Scope<'_>, span: Span) -> Result<(), Error> {
    for part in &text.parts {
        if let Part::Var { name, .. } = part
            && let Some((block, key)) = name.split_once('.')
        {
            let value = scope
                .lookup(block, key)
                .ok_or_else(|| Error::new(format!("`${{{name}}}` names no data value"), span))?;
            if matches!(value.kind, ValueKind::List(_)) {
                return Err(Error::new(
                    format!("`${{{name}}}` is a list, and only a scalar can stand in a command"),
                    span,
                ));
            }
        }
    }
    Ok(())
}

fn check_attrs(attrs: &[Attr], scope: &Scope<'_>, bound: &[String]) -> Result<(), Error> {
    for attr in attrs {
        for arg in &attr.args {
            check_arg_value(&arg.value, scope, bound)?;
        }
    }
    Ok(())
}

fn check_arg_value(value: &Value, scope: &Scope<'_>, bound: &[String]) -> Result<(), Error> {
    match &value.kind {
        ValueKind::Path(block, key) => {
            let target = scope.get(block, key)?;
            if matches!(target.kind, ValueKind::List(_)) {
                return Err(Error::new(
                    format!(
                        "`{}.{}` is a list, and an attribute argument takes one value",
                        block.text, key.text
                    ),
                    value.span,
                ));
            }
        }
        ValueKind::Var(name) => {
            if !bound.iter().any(|b| b == name) {
                return Err(Error::new(
                    format!("`${name}` is not bound by a sweep here"),
                    value.span,
                ));
            }
        }
        ValueKind::Str(text) => check_text(text, scope, value.span)?,
        ValueKind::List(elems) => {
            for elem in elems {
                if let Elem::Value(v) = elem {
                    check_arg_value(v, scope, bound)?;
                }
            }
        }
        ValueKind::Tuple(values) => {
            for v in values {
                check_arg_value(v, scope, bound)?;
            }
        }
        ValueKind::Call(_, args) => {
            for arg in args {
                check_arg_value(&arg.value, scope, bound)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// The names a parameter list binds, once each binding is checked
/// against the data in scope and the names already bound.
fn check_params(
    params: &Params,
    scope: &Scope<'_>,
    bound: &[String],
) -> Result<Vec<String>, Error> {
    let mut names: Vec<String> = Vec::new();
    for binding in &params.bindings {
        let list = match &binding.list.kind {
            ValueKind::Path(block, key) => scope.get(block, key)?,
            ValueKind::List(_) => &binding.list,
            _ => {
                return Err(Error::new("a parameter takes a list", binding.list.span));
            }
        };
        let ValueKind::List(elems) = &list.kind else {
            return Err(Error::new(
                "a parameter takes a list, and this names a scalar",
                binding.list.span,
            ));
        };
        let values: Vec<&Value> = elems
            .iter()
            .map(|elem| match elem {
                Elem::Value(v) => v,
                Elem::Range(_) => unreachable!("ranges are evaluated before checking"),
            })
            .collect();
        if values.is_empty() {
            return Err(Error::new("a parameter's list is empty", binding.list.span));
        }
        let tuples = values
            .iter()
            .filter(|v| matches!(v.kind, ValueKind::Tuple(_)))
            .count();
        let pattern_names: Vec<&Name> = match &binding.pattern {
            Pattern::One(name) => {
                if tuples > 0 {
                    return Err(Error::new(
                        format!(
                            "the list holds tuples, so `{}` needs the pattern form: <(A, B) = …>",
                            name.text
                        ),
                        binding.span,
                    ));
                }
                vec![name]
            }
            Pattern::Tuple(pattern) => {
                if tuples != values.len() {
                    return Err(Error::new(
                        "a tuple pattern takes a list of tuples",
                        binding.list.span,
                    ));
                }
                for v in &values {
                    if let ValueKind::Tuple(items) = &v.kind
                        && items.len() != pattern.len()
                    {
                        return Err(Error::new(
                            format!(
                                "the pattern names {} values and this tuple holds {}",
                                pattern.len(),
                                items.len()
                            ),
                            v.span,
                        ));
                    }
                }
                pattern.iter().collect()
            }
        };
        for name in pattern_names {
            check_reserved(name)?;
            if bound.contains(&name.text) {
                return Err(Error::new(
                    format!("`{}` is already bound by an enclosing sweep", name.text),
                    name.span,
                ));
            }
            if names.contains(&name.text) {
                return Err(Error::new(
                    format!("`{}` is bound twice in one parameter list", name.text),
                    name.span,
                ));
            }
            names.push(name.text.clone());
        }
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    fn resolved(src: &str) -> File {
        let mut file = parse(src).unwrap();
        match resolve(&mut file) {
            Ok(()) => file,
            Err(e) => panic!("{}", e.render("test", src)),
        }
    }

    fn err(src: &str) -> String {
        let mut file = parse(src).unwrap();
        resolve(&mut file).unwrap_err().message
    }

    /// The values of the first data entry of the first data block.
    fn first_list(src: &str) -> Vec<String> {
        let file = resolved(src);
        let Item::Data(data) = &file.items[0] else {
            panic!()
        };
        let ValueKind::List(elems) = &data.entries[0].value.kind else {
            panic!()
        };
        elems
            .iter()
            .map(|e| match e {
                Elem::Value(v) => match &v.kind {
                    ValueKind::Int(n) => n.to_string(),
                    ValueKind::Float { text, .. } => text.clone(),
                    _ => "?".into(),
                },
                Elem::Range(_) => "range".into(),
            })
            .collect()
    }

    const P: &str = " pipeline p { step s { a; } }";

    #[test]
    fn ranges() {
        assert_eq!(
            first_list(&format!("data d {{ N = [0..5]; }}{P}")),
            ["0", "1", "2", "3", "4"]
        );
        assert_eq!(
            first_list(&format!("data d {{ N = [0..=5:2]; }}{P}")),
            ["0", "2", "4"]
        );
        assert_eq!(
            first_list(&format!("data d {{ N = [1..=16:*2]; }}{P}")),
            ["1", "2", "4", "8", "16"]
        );
        assert_eq!(
            first_list(&format!("data d {{ N = [1, 2, 4..=8, 16]; }}{P}")),
            ["1", "2", "4", "5", "6", "7", "8", "16"]
        );
        assert_eq!(
            first_list(&format!("data d {{ N = [0.0..=1.0:0.1]; }}{P}")),
            [
                "0.0", "0.1", "0.2", "0.3", "0.4", "0.5", "0.6", "0.7", "0.8", "0.9", "1.0"
            ]
        );
        assert_eq!(
            first_list(&format!("data d {{ N = [0.0..1.0:0.25]; }}{P}")),
            ["0.00", "0.25", "0.50", "0.75"]
        );
        assert_eq!(
            first_list(&format!("data d {{ N = [1.0..=8.0:*2.0]; }}{P}")),
            ["1.0", "2.0", "4.0", "8.0"]
        );
    }

    #[test]
    fn range_errors() {
        assert_eq!(
            err(&format!("data d {{ N = [5..1]; }}{P}")),
            "a range must not run backwards"
        );
        assert_eq!(
            err(&format!("data d {{ N = [1..5:0]; }}{P}")),
            "a range step must be positive"
        );
        assert_eq!(
            err(&format!("data d {{ N = [1..5:*1]; }}{P}")),
            "a multiplying step must be greater than one"
        );
        assert_eq!(
            err(&format!("data d {{ N = [0..1.0]; }}{P}")),
            "a range runs between two integers or two floats"
        );
        assert_eq!(
            err(&format!("data d {{ N = [0.0..1.0]; }}{P}")),
            "a float range needs a step"
        );
        assert_eq!(
            err(&format!("data d {{ N = [1..5:0.5]; }}{P}")),
            "an integer range takes an integer step"
        );
        assert_eq!(
            err(&format!("data d {{ N = [10..1:*2]; }}{P}")),
            "a range must not run backwards"
        );
        assert_eq!(
            first_list(&format!("data d {{ N = [1.0..=10.0:*1.5]; }}{P}")),
            ["1.0", "1.5", "2.25", "3.375", "5.0625", "7.59375"]
        );
    }

    #[test]
    fn references() {
        resolved(
            "data d { N = [1]; S = \"x\"; } #[stderr_dir(d.S)] pipeline p { step s<d.N> { a ${d.S} $N; } }",
        );
        assert_eq!(
            err("pipeline p { step s<q.N> { a; } }"),
            "no data block named `q`"
        );
        assert_eq!(
            err("data q { K = [1]; } pipeline p { step s<q.N> { a; } }"),
            "data block `q` has no `N`"
        );
        assert_eq!(
            err("data q { S = \"x\"; } pipeline p { step s<q.S> { a; } }"),
            "a parameter takes a list, and this names a scalar"
        );
        assert_eq!(
            err("data q { N = [1]; } #[pool(q.N)] pipeline p { step s { a; } }"),
            "`q.N` is a list, and an attribute argument takes one value"
        );
        assert_eq!(
            err("pipeline p { step s { a ${q.N}; } }"),
            "`${q.N}` names no data value"
        );
        assert_eq!(
            err("data q { N = [1]; } pipeline p { step s { a ${q.N}; } }"),
            "`${q.N}` is a list, and only a scalar can stand in a command"
        );
        assert_eq!(
            err("pipeline p { #[cores($T)] step s { a; } }"),
            "`$T` is not bound by a sweep here"
        );
        resolved("pipeline p { <T = [1]> { #[cores($T)] step s { a; } } }");
        resolved("pipeline p { #[cores($T)] <T = [1]> { step s { a; } } }");
        resolved("pipeline p { step s { #[timeout($T)] <T = [1s]> { a; } } }");
    }

    #[test]
    fn list_shapes() {
        resolved(&format!(
            "data d {{ N = [(1, a), (2, b)]; M = [1, 2]; }}{P}"
        ));
        assert_eq!(
            err(&format!("data d {{ N = [(1, a), (2)]; }}{P}")),
            "this tuple holds 1 values where the list's first holds 2"
        );
        assert_eq!(
            err(&format!("data d {{ N = [(1, a), 2]; }}{P}")),
            "a list holds tuples or plain values, not both"
        );
        assert_eq!(
            err(&format!("data d {{ N = [1, (2, b)]; }}{P}")),
            "a list holds tuples or plain values, not both"
        );
        assert_eq!(
            err(&format!("data d {{ A = 1; B = d.A; }}{P}")),
            "a data value is a number, a string, an identifier, a list or a tuple"
        );
        assert_eq!(
            err(&format!("data d {{ A = [1, (2, $T)]; }}{P}")),
            "a data value is a number, a string, an identifier, a list or a tuple"
        );
        assert_eq!(
            err(&format!("data d {{ A = [file(\"x\")]; }}{P}")),
            "a data value is a number, a string, an identifier, a list or a tuple"
        );
    }

    #[test]
    fn blocks_and_pipelines() {
        assert_eq!(err("data d { N = [1]; }"), "file has no pipeline");
        assert!(
            err("data d { N = [1]; } data d { N = [1]; } pipeline p { step s { a; } }")
                .starts_with("data block `d` is already defined")
        );
        assert!(
            err("data d { N = [1]; } pipeline p { data d { N = [1]; } step s { a; } }")
                .starts_with("data block `d` is already defined")
        );
        assert_eq!(
            err("data d { N = [1]; N = [2]; } pipeline p { step s { a; } }"),
            "`N` is already defined in data block `d`"
        );
        assert_eq!(
            err("data a { b_c = [1]; } data a_b { c = [2]; } pipeline p { step s { a; } }"),
            "`a_b.c` and `a.b_c` both become `a_b_c` in the script"
        );
        assert_eq!(
            err("data a { X = [(1, 2)]; X_1 = [3]; } pipeline p { step s { a; } }"),
            "`a.X_1` and `a.X` both become `a_X_1` in the script"
        );
        assert_eq!(
            err("data _a { X = [1]; } pipeline p { step s { a; } }"),
            "`_a` starts with `_`, which is reserved for the script"
        );
        assert_eq!(
            err("data a { _x = [1]; } pipeline p { step s { a; } }"),
            "`_x` starts with `_`, which is reserved for the script"
        );
    }

    #[test]
    fn parameter_lists() {
        assert_eq!(
            err("data d { P = [(1, a)]; } pipeline p { step s<d.P> { a; } }"),
            "the list holds tuples, so `P` needs the pattern form: <(A, B) = …>"
        );
        assert_eq!(
            err("data d { N = [1]; } pipeline p { step s<(A, B) = d.N> { a; } }"),
            "a tuple pattern takes a list of tuples"
        );
        assert_eq!(
            err("pipeline p { step s<(A, B) = [(1, a), (2, b, c)]> { a; } }"),
            "this tuple holds 3 values where the list's first holds 2"
        );
        assert_eq!(
            err("pipeline p { step s<N = [1]> { <N = [2]> { a; } } }"),
            "`N` is already bound by an enclosing sweep"
        );
        assert_eq!(
            err("pipeline p { step s<N = [1], N = [2]> { a; } }"),
            "`N` is bound twice in one parameter list"
        );
        assert_eq!(
            err("pipeline p { step s<N = []> { a; } }"),
            "a parameter's list is empty"
        );
        assert_eq!(
            err("pipeline p { <T = [1]> { step s<N = [$T]> { a; } } }"),
            "a parameter's list is a number, a string, an identifier, a list or a tuple"
        );
        assert_eq!(
            err("pipeline p { <T = [1]> { step s<(A, B) = [($T, 1)]> { a; } } }"),
            "a parameter's list is a number, a string, an identifier, a list or a tuple"
        );
        assert_eq!(
            err("data d { X = [1]; } pipeline p { step s<N = [d.X]> { a; } }"),
            "a parameter's list is a number, a string, an identifier, a list or a tuple"
        );
        assert_eq!(
            err("pipeline p { step s<_i = [1]> { a; } }"),
            "`_i` starts with `_`, which is reserved for the script"
        );
        assert_eq!(
            err("pipeline p { step s<N = [(1, a), (2)]> { a; } }"),
            "this tuple holds 1 values where the list's first holds 2"
        );
    }
}
