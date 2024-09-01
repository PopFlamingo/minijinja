use std::collections::BTreeMap;
use std::mem;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::compiler::instructions::{
    Instruction, Instructions, LOOP_FLAG_RECURSIVE, LOOP_FLAG_WITH_LOOP_VAR, MAX_LOCALS,
};
use crate::environment::Environment;
use crate::error::{Error, ErrorKind};
use crate::output::{CaptureMode, Output};
use crate::utils::{untrusted_size_hint, AutoEscape, UndefinedBehavior};
use crate::value::namespace_object::Namespace;
use crate::value::{ops, value_map_with_capacity, value_optimization, Kwargs, Value};
use crate::vm::context::{Frame, LoopState, Stack};
use crate::vm::loop_object::Loop;
use crate::vm::state::BlockStack;

#[cfg(feature = "macros")]
use crate::vm::closure_object::Closure;

pub(crate) use crate::vm::context::Context;
pub use crate::vm::state::State;

#[cfg(feature = "macros")]
mod closure_object;
mod context;
#[cfg(feature = "fuel")]
mod fuel;
mod loop_object;
#[cfg(feature = "macros")]
mod macro_object;
mod state;

// the cost of a single include against the stack limit.
#[cfg(feature = "multi_template")]
const INCLUDE_RECURSION_COST: usize = 10;

// the cost of a single macro call against the stack limit.
#[cfg(feature = "macros")]
const MACRO_RECURSION_COST: usize = 4;

/// Helps to evaluate something.
#[cfg_attr(feature = "internal_debug", derive(Debug))]
pub struct Vm<'env> {
    env: &'env Environment<'env>,
}

pub(crate) fn prepare_blocks<'env, 'template>(
    blocks: &'template BTreeMap<&'env str, Instructions<'env>>,
) -> BTreeMap<&'env str, BlockStack<'template, 'env>> {
    blocks
        .iter()
        .map(|(name, instr)| (*name, BlockStack::new(instr)))
        .collect()
}

fn get_or_lookup_local<T, F>(vec: &mut [Option<T>], local_id: u8, f: F) -> Option<T>
where
    T: Copy,
    F: FnOnce() -> Option<T>,
{
    if local_id == !0 {
        f()
    } else if let Some(Some(rv)) = vec.get(local_id as usize) {
        Some(*rv)
    } else {
        let val = some!(f());
        vec[local_id as usize] = Some(val);
        Some(val)
    }
}

impl<'env> Vm<'env> {
    /// Creates a new VM.
    pub fn new(env: &'env Environment<'env>) -> Vm<'env> {
        Vm { env }
    }

    /// Evaluates the given inputs.
    ///
    /// It returns both the last value left on the stack as well as the state
    /// at the end of the evaluation.
    pub fn eval<'template>(
        &self,
        instructions: &'template Instructions<'env>,
        root: Value,
        blocks: &'template BTreeMap<&'env str, Instructions<'env>>,
        out: &mut Output,
        auto_escape: AutoEscape,
    ) -> Result<(Option<Value>, State<'template, 'env>), Error> {
        let _guard = value_optimization();
        let mut state = State::new(
            self.env,
            Context::new_with_frame(ok!(Frame::new_checked(root)), self.env.recursion_limit()),
            auto_escape,
            instructions,
            prepare_blocks(blocks),
        );
        self.eval_state(&mut state, out).map(|x| (x, state))
    }

    /// Evaluate a macro in a state.
    #[cfg(feature = "macros")]
    #[allow(clippy::too_many_arguments)]
    pub fn eval_macro(
        &self,
        instructions: &Instructions<'env>,
        pc: usize,
        closure: Value,
        context_base: Value,
        caller: Option<Value>,
        out: &mut Output,
        state: &State,
        args: Vec<Value>,
    ) -> Result<Option<Value>, Error> {
        let mut ctx = Context::new_with_frame(Frame::new(context_base), self.env.recursion_limit());
        ok!(ctx.push_frame(Frame::new(closure)));
        if let Some(caller) = caller {
            ctx.store("caller", caller);
        }
        ok!(ctx.incr_depth(state.ctx.depth() + MACRO_RECURSION_COST));
        self.do_eval(
            &mut State {
                env: self.env,
                ctx,
                current_block: None,
                auto_escape: state.auto_escape(),
                instructions,
                blocks: BTreeMap::default(),
                loaded_templates: Default::default(),
                #[cfg(feature = "macros")]
                id: state.id,
                #[cfg(feature = "macros")]
                macros: state.macros.clone(),
                #[cfg(feature = "macros")]
                closure_tracker: state.closure_tracker.clone(),
                #[cfg(feature = "fuel")]
                fuel_tracker: state.fuel_tracker.clone(),
            },
            out,
            Stack::from(args),
            pc,
        )
    }

    /// This is the actual evaluation loop that works with a specific context.
    #[inline(always)]
    fn eval_state(
        &self,
        state: &mut State<'_, 'env>,
        out: &mut Output,
    ) -> Result<Option<Value>, Error> {
        self.do_eval(state, out, Stack::default(), 0)
    }

    /// Performs the actual evaluation, optionally with stack growth functionality.
    fn do_eval(
        &self,
        state: &mut State<'_, 'env>,
        out: &mut Output,
        stack: Stack,
        pc: usize,
    ) -> Result<Option<Value>, Error> {
        #[cfg(feature = "stacker")]
        {
            stacker::maybe_grow(32 * 1024, 1024 * 1024, || {
                self.eval_impl(state, out, stack, pc)
            })
        }
        #[cfg(not(feature = "stacker"))]
        {
            self.eval_impl(state, out, stack, pc)
        }
    }

    #[inline]
    fn eval_impl(
        &self,
        state: &mut State<'_, 'env>,
        out: &mut Output,
        mut stack: Stack,
        mut pc: usize,
    ) -> Result<Option<Value>, Error> {
        let initial_auto_escape = state.auto_escape;
        let undefined_behavior = state.undefined_behavior();
        let mut auto_escape_stack = vec![];
        let mut next_loop_recursion_jump = None;
        let mut loaded_filters = [None; MAX_LOCALS];
        let mut loaded_tests = [None; MAX_LOCALS];

        // If we are extending we are holding the instructions of the target parent
        // template here.  This is used to detect multiple extends and the evaluation
        // uses these instructions when it makes it to the end of the instructions.
        #[cfg(feature = "multi_template")]
        let mut parent_instructions = None;

        macro_rules! recurse_loop {
            ($capture:expr) => {{
                let jump_target = ctx_ok!(self.prepare_loop_recursion(state));
                // the way this works is that we remember the next instruction
                // as loop exit jump target.  Whenever a loop is pushed, it
                // memorizes the value in `next_loop_iteration_jump` to jump
                // to.
                next_loop_recursion_jump = Some((pc + 1, $capture));
                if $capture {
                    out.begin_capture(CaptureMode::Capture);
                }
                pc = jump_target;
                continue;
            }};
        }

        // looks nicer this way
        #[allow(clippy::while_let_loop)]
        loop {
            let instr = match state.instructions.get(pc) {
                Some(instr) => instr,
                #[cfg(not(feature = "multi_template"))]
                None => break,
                #[cfg(feature = "multi_template")]
                None => {
                    // when an extends statement appears in a template, when we hit the
                    // last instruction we need to check if parent instructions were
                    // stashed away (which means we found an extends tag which invoked
                    // `LoadBlocks`).  If we do find instructions, we reset back to 0
                    // from the new instructions.
                    state.instructions = match parent_instructions.take() {
                        Some(instr) => instr,
                        None => break,
                    };
                    out.end_capture(AutoEscape::None);
                    pc = 0;
                    // because we swap out the instructions we also need to unload all
                    // the filters and tests to ensure that we are not accidentally
                    // reusing the local_ids for completely different filters.
                    loaded_filters = [None; MAX_LOCALS];
                    loaded_tests = [None; MAX_LOCALS];
                    continue;
                }
            };

            // if we only have two arguments that we pull from the stack, we
            // can assign them to a and b.  This slightly reduces the amount of
            // code bloat generated here.
            let a;
            let b;

            macro_rules! func_binop {
                ($method:ident) => {{
                    b = stack.pop();
                    a = stack.pop();
                    stack.push(ctx_ok!(ops::$method(&a, &b)));
                }};
            }

            macro_rules! op_binop {
                ($op:tt) => {{
                    b = stack.pop();
                    a = stack.pop();
                    stack.push(Value::from(a $op b));
                }};
            }

            macro_rules! bail {
                ($err:expr) => {{
                    let mut err = $err;
                    process_err(&mut err, pc, state);
                    return Err(err);
                }};
            }

            macro_rules! ctx_ok {
                ($expr:expr) => {
                    match $expr {
                        Ok(rv) => rv,
                        Err(err) => bail!(err),
                    }
                };
            }

            macro_rules! assert_valid {
                ($expr:expr) => {{
                    let val = $expr;
                    match val.validate() {
                        Ok(val) => val,
                        Err(err) => bail!(err),
                    }
                }};
            }

            // if the fuel consumption feature is enabled, track the fuel
            // consumption here.
            #[cfg(feature = "fuel")]
            if let Some(ref tracker) = state.fuel_tracker {
                ctx_ok!(tracker.track(instr));
            }

            match instr {
                Instruction::Swap => {
                    let a = stack.pop();
                    let b = stack.pop();
                    stack.push(a);
                    stack.push(b);
                }
                Instruction::EmitRaw(val) => {
                    // this only produces a format error, no need to attach
                    // location information.
                    ok!(out.write_str(val).map_err(Error::from));
                }
                Instruction::Emit => {
                    ctx_ok!(self.env.format(&stack.pop(), state, out));
                }
                Instruction::StoreLocal(name) => {
                    state.ctx.store(name, stack.pop());
                }
                Instruction::Lookup(name) => {
                    stack.push(assert_valid!(state
                        .lookup(name)
                        .unwrap_or(Value::UNDEFINED)));
                }
                Instruction::GetAttr(name) => {
                    a = stack.pop();
                    // This is a common enough operation that it's interesting to consider a fast
                    // path here.  This is slightly faster than the regular attr lookup because we
                    // do not need to pass down the error object for the more common success case.
                    // Only when we cannot look up something, we start to consider the undefined
                    // special case.
                    stack.push(match a.get_attr_fast(name) {
                        Some(value) => assert_valid!(value),
                        None => ctx_ok!(undefined_behavior.handle_undefined(a.is_undefined())),
                    });
                }
                Instruction::SetAttr(name) => {
                    b = stack.pop();
                    a = stack.pop();
                    if let Some(ns) = b.downcast_object_ref::<Namespace>() {
                        ns.set_value(name, a);
                    } else {
                        bail!(Error::new(
                            ErrorKind::InvalidOperation,
                            format!("can only assign to namespaces, not {}", b.kind())
                        ));
                    }
                }
                Instruction::GetItem => {
                    a = stack.pop();
                    b = stack.pop();
                    stack.push(match b.get_item_opt(&a) {
                        Some(value) => assert_valid!(value),
                        None => ctx_ok!(undefined_behavior.handle_undefined(b.is_undefined())),
                    });
                }
                Instruction::Slice => {
                    let step = stack.pop();
                    let stop = stack.pop();
                    b = stack.pop();
                    a = stack.pop();
                    if a.is_undefined() && matches!(undefined_behavior, UndefinedBehavior::Strict) {
                        bail!(Error::from(ErrorKind::UndefinedError));
                    }
                    stack.push(ctx_ok!(ops::slice(a, b, stop, step)));
                }
                Instruction::LoadConst(value) => {
                    stack.push(value.clone());
                }
                Instruction::BuildMap(pair_count) => {
                    let mut map = value_map_with_capacity(*pair_count);
                    for _ in 0..*pair_count {
                        let value = stack.pop();
                        let key = stack.pop();
                        map.insert(key, value);
                    }
                    stack.push(Value::from_object(map))
                }
                Instruction::BuildKwargs(pair_count) => {
                    let mut map = value_map_with_capacity(*pair_count);
                    for _ in 0..*pair_count {
                        let value = stack.pop();
                        let key = stack.pop();
                        map.insert(key, value);
                    }
                    stack.push(Kwargs::wrap(map))
                }
                Instruction::BuildList(n) => {
                    let count = n.unwrap_or_else(|| stack.pop().try_into().unwrap());
                    let mut v = Vec::with_capacity(untrusted_size_hint(count));
                    for _ in 0..count {
                        v.push(stack.pop());
                    }
                    v.reverse();
                    stack.push(Value::from_object(v))
                }
                Instruction::UnpackList(count) => {
                    ctx_ok!(self.unpack_list(&mut stack, *count));
                }
                Instruction::Add => func_binop!(add),
                Instruction::Sub => func_binop!(sub),
                Instruction::Mul => func_binop!(mul),
                Instruction::Div => func_binop!(div),
                Instruction::IntDiv => func_binop!(int_div),
                Instruction::Rem => func_binop!(rem),
                Instruction::Pow => func_binop!(pow),
                Instruction::Eq => op_binop!(==),
                Instruction::Ne => op_binop!(!=),
                Instruction::Gt => op_binop!(>),
                Instruction::Gte => op_binop!(>=),
                Instruction::Lt => op_binop!(<),
                Instruction::Lte => op_binop!(<=),
                Instruction::Not => {
                    a = stack.pop();
                    stack.push(Value::from(!a.is_true()));
                }
                Instruction::StringConcat => {
                    a = stack.pop();
                    b = stack.pop();
                    stack.push(ops::string_concat(b, &a));
                }
                Instruction::In => {
                    a = stack.pop();
                    b = stack.pop();
                    // the in-operator can fail if the value is undefined and
                    // we are in strict mode.
                    ctx_ok!(state.undefined_behavior().assert_iterable(&a));
                    stack.push(ctx_ok!(ops::contains(&a, &b)));
                }
                Instruction::Neg => {
                    a = stack.pop();
                    stack.push(ctx_ok!(ops::neg(&a)));
                }
                Instruction::PushWith => {
                    ctx_ok!(state.ctx.push_frame(Frame::default()));
                }
                Instruction::PopFrame => {
                    if let Some(mut loop_ctx) = state.ctx.pop_frame().current_loop {
                        if let Some((target, end_capture)) = loop_ctx.current_recursion_jump.take()
                        {
                            pc = target;
                            if end_capture {
                                stack.push(out.end_capture(state.auto_escape));
                            }
                            continue;
                        }
                    }
                }
                #[cfg(feature = "macros")]
                Instruction::IsUndefined => {
                    a = stack.pop();
                    stack.push(Value::from(a.is_undefined()));
                }
                Instruction::PushLoop(flags) => {
                    a = stack.pop();
                    ctx_ok!(self.push_loop(state, a, *flags, pc, next_loop_recursion_jump.take()));
                }
                Instruction::Iterate(jump_target) => {
                    let l = state.ctx.current_loop().unwrap();
                    l.object.idx.fetch_add(1, Ordering::Relaxed);

                    let next = {
                        #[cfg(feature = "adjacent_loop_items")]
                        {
                            let mut triple = l.object.value_triple.lock().unwrap();
                            triple.0 = triple.1.take();
                            triple.1 = triple.2.take();
                            triple.2 = l.iterator.next();
                            triple.1.clone()
                        }
                        #[cfg(not(feature = "adjacent_loop_items"))]
                        {
                            l.iterator.next()
                        }
                    };
                    match next {
                        Some(item) => stack.push(assert_valid!(item)),
                        None => {
                            pc = *jump_target;
                            continue;
                        }
                    };
                }
                Instruction::PushDidNotIterate => {
                    let l = state.ctx.current_loop().unwrap();
                    stack.push(Value::from(l.object.idx.load(Ordering::Relaxed) == 0));
                }
                Instruction::Jump(jump_target) => {
                    pc = *jump_target;
                    continue;
                }
                Instruction::JumpIfFalse(jump_target) => {
                    a = stack.pop();
                    if !ok!(undefined_behavior.is_true(&a)) {
                        pc = *jump_target;
                        continue;
                    }
                }
                Instruction::JumpIfFalseOrPop(jump_target) => {
                    if !ok!(undefined_behavior.is_true(stack.peek())) {
                        pc = *jump_target;
                        continue;
                    } else {
                        stack.pop();
                    }
                }
                Instruction::JumpIfTrueOrPop(jump_target) => {
                    if ok!(undefined_behavior.is_true(stack.peek())) {
                        pc = *jump_target;
                        continue;
                    } else {
                        stack.pop();
                    }
                }
                #[cfg(feature = "multi_template")]
                Instruction::CallBlock(name) => {
                    if parent_instructions.is_none() && !out.is_discarding() {
                        self.call_block(name, state, out)?;
                    }
                }
                Instruction::PushAutoEscape => {
                    a = stack.pop();
                    auto_escape_stack.push(state.auto_escape);
                    state.auto_escape = ctx_ok!(self.derive_auto_escape(a, initial_auto_escape));
                }
                Instruction::PopAutoEscape => {
                    state.auto_escape = auto_escape_stack.pop().unwrap();
                }
                Instruction::BeginCapture(mode) => {
                    out.begin_capture(*mode);
                }
                Instruction::EndCapture => {
                    stack.push(out.end_capture(state.auto_escape));
                }
                Instruction::ApplyFilter(name, arg_count, local_id) => {
                    let filter =
                        ctx_ok!(get_or_lookup_local(&mut loaded_filters, *local_id, || {
                            state.env.get_filter(name)
                        })
                        .ok_or_else(|| {
                            Error::new(
                                ErrorKind::UnknownFilter,
                                format!("filter {name} is unknown"),
                            )
                        }));
                    let args = stack.slice_top(*arg_count);
                    a = ctx_ok!(filter.apply_to(state, args));
                    stack.drop_top(*arg_count);
                    stack.push(a);
                }
                Instruction::PerformTest(name, arg_count, local_id) => {
                    let test = ctx_ok!(get_or_lookup_local(&mut loaded_tests, *local_id, || {
                        state.env.get_test(name)
                    })
                    .ok_or_else(|| {
                        Error::new(ErrorKind::UnknownTest, format!("test {name} is unknown"))
                    }));
                    let args = stack.slice_top(*arg_count);
                    let rv = ctx_ok!(test.perform(state, args));
                    stack.drop_top(*arg_count);
                    stack.push(Value::from(rv));
                }
                Instruction::CallFunction(name, arg_count) => {
                    // super is a special function reserved for super-ing into blocks.
                    if *name == "super" {
                        if *arg_count != 0 {
                            bail!(Error::new(
                                ErrorKind::InvalidOperation,
                                "super() takes no arguments",
                            ));
                        }
                        stack.push(ctx_ok!(self.perform_super(state, out, true)));
                    // loop is a special name which when called recurses the current loop.
                    } else if *name == "loop" {
                        if *arg_count != 1 {
                            bail!(Error::new(
                                ErrorKind::InvalidOperation,
                                format!("loop() takes one argument, got {}", *arg_count)
                            ));
                        }
                        // leave the one argument on the stack for the recursion
                        recurse_loop!(true);
                    } else if let Some(func) = state.lookup(name) {
                        let args = stack.slice_top(*arg_count);
                        a = ctx_ok!(func.call(state, args));
                        stack.drop_top(*arg_count);
                        stack.push(a);
                    } else {
                        bail!(Error::new(
                            ErrorKind::UnknownFunction,
                            format!("{name} is unknown"),
                        ));
                    }
                }
                Instruction::CallMethod(name, arg_count) => {
                    let args = stack.slice_top(*arg_count);
                    a = ctx_ok!(args[0].call_method(state, name, &args[1..]));
                    stack.drop_top(*arg_count);
                    stack.push(a);
                }
                Instruction::CallObject(arg_count) => {
                    let args = stack.slice_top(*arg_count);
                    a = ctx_ok!(args[0].call(state, &args[1..]));
                    stack.drop_top(*arg_count);
                    stack.push(a);
                }
                Instruction::DupTop => {
                    stack.push(stack.peek().clone());
                }
                Instruction::DiscardTop => {
                    stack.pop();
                }
                Instruction::FastSuper => {
                    ctx_ok!(self.perform_super(state, out, false));
                }
                Instruction::FastRecurse => {
                    recurse_loop!(false);
                }
                // Explanation on the behavior of `LoadBlocks` and rendering of
                // inherited templates:
                //
                // MiniJinja inherits the behavior from Jinja2 where extending
                // loads the blocks (`LoadBlocks`) and the rest of the template
                // keeps executing but with output disabled, only at the end the
                // parent template is then invoked.  This has the effect that
                // you can still set variables or declare macros and that they
                // become visible in the blocks.
                //
                // This behavior has a few downsides.  First of all what happens
                // in the parent template overrides what happens in the child.
                // For instance if you declare a macro named `foo` after `{%
                // extends %}` and then a variable with that named is also set
                // in the parent template, then you won't be able to call that
                // macro in the body.
                //
                // The reason for this is that blocks unlike macros do not have
                // closures in Jinja2/MiniJinja.
                //
                // However for the common case this is convenient because it
                // lets you put some imports there and for as long as you do not
                // create name clashes this works fine.
                #[cfg(feature = "multi_template")]
                Instruction::LoadBlocks => {
                    a = stack.pop();
                    if parent_instructions.is_some() {
                        bail!(Error::new(
                            ErrorKind::InvalidOperation,
                            "tried to extend a second time in a template"
                        ));
                    }
                    parent_instructions = Some(ctx_ok!(self.load_blocks(a, state)));
                    out.begin_capture(CaptureMode::Discard);
                }
                #[cfg(feature = "multi_template")]
                Instruction::Include(ignore_missing) => {
                    a = stack.pop();
                    ctx_ok!(self.perform_include(a, state, out, *ignore_missing));
                }
                #[cfg(feature = "multi_template")]
                Instruction::ExportLocals => {
                    let locals = state.ctx.current_locals_mut();
                    let mut module = value_map_with_capacity(locals.len());
                    for (key, value) in locals.iter() {
                        module.insert(Value::from(*key), value.clone());
                    }
                    stack.push(Value::from_object(module));
                }
                #[cfg(feature = "macros")]
                Instruction::BuildMacro(name, offset, flags) => {
                    self.build_macro(&mut stack, state, *offset, name, *flags);
                }
                #[cfg(feature = "macros")]
                Instruction::Return => break,
                #[cfg(feature = "macros")]
                Instruction::Enclose(name) => {
                    // the first time we enclose a value, we need to create a closure
                    // and store it on the context, and add it to the closure tracker
                    // for cycle breaking.
                    if state.ctx.closure().is_none() {
                        let closure = Arc::new(Closure::default());
                        state.closure_tracker.track_closure(closure.clone());
                        state.ctx.reset_closure(Some(closure));
                    }
                    state.ctx.enclose(state.env, name);
                }
                #[cfg(feature = "macros")]
                Instruction::GetClosure => {
                    stack.push(
                        state
                            .ctx
                            .closure()
                            .map_or(Value::UNDEFINED, |x| Value::from_dyn_object(x.clone())),
                    );
                }
            }
            pc += 1;
        }

        Ok(stack.try_pop())
    }

    #[cfg(feature = "multi_template")]
    fn perform_include(
        &self,
        name: Value,
        state: &mut State<'_, 'env>,
        out: &mut Output,
        ignore_missing: bool,
    ) -> Result<(), Error> {
        let obj = name.as_object();
        let choices = obj
            .as_ref()
            .and_then(|d| d.try_iter())
            .into_iter()
            .flatten()
            .chain(obj.is_none().then(|| name.clone()));

        let mut templates_tried = vec![];
        for choice in choices {
            let name = ok!(choice.as_str().ok_or_else(|| {
                Error::new(
                    ErrorKind::InvalidOperation,
                    "template name was not a string",
                )
            }));
            let tmpl = match state.get_template(name) {
                Ok(tmpl) => tmpl,
                Err(err) => {
                    if err.kind() == ErrorKind::TemplateNotFound {
                        templates_tried.push(choice);
                    } else {
                        return Err(err);
                    }
                    continue;
                }
            };

            let (new_instructions, new_blocks) = ok!(tmpl.instructions_and_blocks());
            let old_escape = mem::replace(&mut state.auto_escape, tmpl.initial_auto_escape());
            let old_instructions = mem::replace(&mut state.instructions, new_instructions);
            let old_blocks = mem::replace(&mut state.blocks, prepare_blocks(new_blocks));
            // we need to make a copy of the loaded templates here as we want
            // to forget about the templates that an include triggered by the
            // time the include finishes.
            let old_loaded_templates = state.loaded_templates.clone();
            ok!(state.ctx.incr_depth(INCLUDE_RECURSION_COST));
            let rv;
            #[cfg(feature = "macros")]
            {
                let old_closure = state.ctx.take_closure();
                rv = self.eval_state(state, out);
                state.ctx.reset_closure(old_closure);
            }
            #[cfg(not(feature = "macros"))]
            {
                rv = self.eval_state(state, out);
            }
            state.ctx.decr_depth(INCLUDE_RECURSION_COST);
            state.loaded_templates = old_loaded_templates;
            state.auto_escape = old_escape;
            state.instructions = old_instructions;
            state.blocks = old_blocks;
            ok!(rv.map_err(|err| {
                Error::new(
                    ErrorKind::BadInclude,
                    format!("error in \"{}\"", tmpl.name()),
                )
                .with_source(err)
            }));
            return Ok(());
        }
        if !templates_tried.is_empty() && !ignore_missing {
            Err(Error::new(
                ErrorKind::TemplateNotFound,
                if templates_tried.len() == 1 {
                    format!(
                        "tried to include non-existing template {:?}",
                        templates_tried[0]
                    )
                } else {
                    format!(
                        "tried to include one of multiple templates, none of which existed {}",
                        Value::from(templates_tried)
                    )
                },
            ))
        } else {
            Ok(())
        }
    }

    fn perform_super(
        &self,
        state: &mut State<'_, 'env>,
        out: &mut Output,
        capture: bool,
    ) -> Result<Value, Error> {
        let name = ok!(state.current_block.ok_or_else(|| {
            Error::new(ErrorKind::InvalidOperation, "cannot super outside of block")
        }));

        let block_stack = state.blocks.get_mut(name).unwrap();
        if !block_stack.push() {
            return Err(Error::new(
                ErrorKind::InvalidOperation,
                "no parent block exists",
            ));
        }

        if capture {
            out.begin_capture(CaptureMode::Capture);
        }

        let old_instructions = mem::replace(&mut state.instructions, block_stack.instructions());
        ok!(state.ctx.push_frame(Frame::default()));
        let rv = self.eval_state(state, out);
        state.ctx.pop_frame();
        state.instructions = old_instructions;
        state.blocks.get_mut(name).unwrap().pop();

        ok!(rv.map_err(|err| {
            Error::new(ErrorKind::EvalBlock, "error in super block").with_source(err)
        }));
        if capture {
            Ok(out.end_capture(state.auto_escape))
        } else {
            Ok(Value::UNDEFINED)
        }
    }

    fn prepare_loop_recursion(&self, state: &mut State) -> Result<usize, Error> {
        if let Some(loop_ctx) = state.ctx.current_loop() {
            if let Some(recurse_jump_target) = loop_ctx.recurse_jump_target {
                Ok(recurse_jump_target)
            } else {
                Err(Error::new(
                    ErrorKind::InvalidOperation,
                    "cannot recurse outside of recursive loop",
                ))
            }
        } else {
            Err(Error::new(
                ErrorKind::InvalidOperation,
                "cannot recurse outside of loop",
            ))
        }
    }

    #[cfg(feature = "multi_template")]
    fn load_blocks(
        &self,
        name: Value,
        state: &mut State<'_, 'env>,
    ) -> Result<&'env Instructions<'env>, Error> {
        let name = match name.as_str() {
            Some(name) => name,
            None => {
                return Err(Error::new(
                    ErrorKind::InvalidOperation,
                    "template name was not a string",
                ))
            }
        };
        if state.loaded_templates.contains(&name) {
            return Err(Error::new(
                ErrorKind::InvalidOperation,
                format!("cycle in template inheritance. {name:?} was referenced more than once"),
            ));
        }
        let tmpl = ok!(state.get_template(name));
        let (new_instructions, new_blocks) = ok!(tmpl.instructions_and_blocks());
        state.loaded_templates.insert(new_instructions.name());
        for (name, instr) in new_blocks.iter() {
            state
                .blocks
                .entry(name)
                .or_default()
                .append_instructions(instr);
        }
        Ok(new_instructions)
    }

    #[cfg(feature = "multi_template")]
    pub(crate) fn call_block(
        &self,
        name: &str,
        state: &mut State<'_, 'env>,
        out: &mut Output,
    ) -> Result<Option<Value>, Error> {
        if let Some((name, block_stack)) = state.blocks.get_key_value(name) {
            let old_block = mem::replace(&mut state.current_block, Some(name));
            let old_instructions =
                mem::replace(&mut state.instructions, block_stack.instructions());
            state.ctx.push_frame(Frame::default())?;
            let rv = self.eval_state(state, out);
            state.ctx.pop_frame();
            state.instructions = old_instructions;
            state.current_block = old_block;
            rv
        } else {
            Err(Error::new(
                ErrorKind::UnknownBlock,
                format!("block '{}' not found", name),
            ))
        }
    }

    fn derive_auto_escape(
        &self,
        value: Value,
        initial_auto_escape: AutoEscape,
    ) -> Result<AutoEscape, Error> {
        match (value.as_str(), value == Value::from(true)) {
            (Some("html"), _) => Ok(AutoEscape::Html),
            #[cfg(feature = "json")]
            (Some("json"), _) => Ok(AutoEscape::Json),
            (Some("none"), _) | (None, false) => Ok(AutoEscape::None),
            (None, true) => Ok(if matches!(initial_auto_escape, AutoEscape::None) {
                AutoEscape::Html
            } else {
                initial_auto_escape
            }),
            _ => Err(Error::new(
                ErrorKind::InvalidOperation,
                "invalid value to autoescape tag",
            )),
        }
    }

    fn push_loop(
        &self,
        state: &mut State<'_, 'env>,
        iterable: Value,
        flags: u8,
        pc: usize,
        current_recursion_jump: Option<(usize, bool)>,
    ) -> Result<(), Error> {
        #[allow(unused_mut)]
        let mut iterator = ok!(state.undefined_behavior().try_iter(iterable));
        // for an iterator where the lower and upper bound are matching we can
        // consider them to have ExactSizeIterator semantics.  We do however not
        // expect ExactSizeIterator bounds themselves to support iteration by
        // other means.
        let len = match iterator.size_hint() {
            (lower, Some(upper)) if lower == upper => Some(lower),
            _ => None,
        };
        let depth = state
            .ctx
            .current_loop()
            .filter(|x| x.recurse_jump_target.is_some())
            .map_or(0, |x| x.object.depth + 1);
        let recursive = flags & LOOP_FLAG_RECURSIVE != 0;
        let with_loop_var = flags & LOOP_FLAG_WITH_LOOP_VAR != 0;
        ok!(state.ctx.push_frame(Frame {
            current_loop: Some(LoopState {
                with_loop_var,
                recurse_jump_target: if recursive { Some(pc) } else { None },
                current_recursion_jump,
                object: Arc::new(Loop {
                    idx: AtomicUsize::new(!0usize),
                    len,
                    depth,
                    #[cfg(feature = "adjacent_loop_items")]
                    value_triple: Mutex::new((None, None, iterator.next())),
                    last_changed_value: Mutex::default(),
                }),
                iterator,
            }),
            ..Frame::default()
        }));
        Ok(())
    }

    fn unpack_list(&self, stack: &mut Stack, count: usize) -> Result<(), Error> {
        let top = stack.pop();
        let iter = ok!(top
            .as_object()
            .and_then(|x| x.try_iter())
            .ok_or_else(|| Error::new(ErrorKind::CannotUnpack, "value is not iterable")));

        let mut n = 0;
        for item in iter {
            stack.push(item);
            n += 1;
        }

        if n == count {
            stack.reverse_top(n);
            Ok(())
        } else {
            Err(Error::new(
                ErrorKind::CannotUnpack,
                format!("sequence of wrong length (expected {}, got {})", count, n,),
            ))
        }
    }

    #[cfg(feature = "macros")]
    fn build_macro(
        &self,
        stack: &mut Stack,
        state: &mut State,
        offset: usize,
        name: &str,
        flags: u8,
    ) {
        use crate::{compiler::instructions::MACRO_CALLER, vm::macro_object::Macro};

        let arg_spec = stack.pop().try_iter().unwrap().collect();
        let closure = stack.pop();
        let macro_ref_id = state.macros.len();
        Arc::make_mut(&mut state.macros).push((state.instructions, offset));
        stack.push(Value::from_object(Macro {
            name: Value::from(name),
            arg_spec,
            macro_ref_id,
            state_id: state.id,
            closure,
            caller_reference: (flags & MACRO_CALLER) != 0,
        }));
    }
}

#[inline(never)]
#[cold]
fn process_err(err: &mut Error, pc: usize, state: &State) {
    // only attach line information if the error does not have line info yet.
    if err.line().is_none() {
        if let Some(span) = state.instructions.get_span(pc) {
            err.set_filename_and_span(state.instructions.name(), span);
        } else if let Some(lineno) = state.instructions.get_line(pc) {
            err.set_filename_and_line(state.instructions.name(), lineno);
        }
    }
    // only attach debug info if we don't have one yet and we are in debug mode.
    #[cfg(feature = "debug")]
    {
        if state.env.debug() && err.debug_info().is_none() {
            err.attach_debug_info(state.make_debug_info(pc, state.instructions));
        }
    }
}
