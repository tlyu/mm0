//! Build documentation pages for MM1/MM0 files
use std::{collections::{hash_map::Entry, HashMap}, hash::Hash, path::PathBuf};
use bit_set::BitSet;
use clap::ArgMatches;
use lsp_types::Url;
use pulldown_cmark::escape::WriteWrapper;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::mem;
use crate::{lisp::pretty::Annot, ArcString, AtomData, AtomId, DeclKey, DocComment, EnvMergeIter,
  Environment, ExprNode, FileRef, FormatEnv, LinedString, LispVal, Proof, ProofNode, SliceUninit,
  StmtTrace, TermId, Thm, ThmId, ThmKind, Type};

const PP_WIDTH: usize = 160;

/// A writer that implements `RenderAnnotated`, which produces HTML text.
struct HtmlPrinter<'a, W: Write> {
  env: &'a Environment,
  mangler: &'a mut Mangler,
  rel: &'static str,
  w: WriteWrapper<&'a mut W>,
  stack: Vec<&'static str>
}

impl<'a, W: Write> HtmlPrinter<'a, W> {
  /// Make a new `HtmlPrinter` from some writer.
  fn new(env: &'a Environment,
      mangler: &'a mut Mangler, w: &'a mut W, rel: &'static str) -> Self {
    HtmlPrinter { env, mangler, w: WriteWrapper(w), rel, stack: Vec::new() }
  }
}

impl<'a, W: Write> pretty::Render for HtmlPrinter<'a, W> {
  type Error = std::io::Error;

  fn write_str(&mut self, s: &str) -> io::Result<usize> {
    pulldown_cmark::escape::escape_html(&mut self.w, s)?;
    Ok(s.len())
  }

  fn fail_doc(&self) -> Self::Error {
    io::Error::new(io::ErrorKind::Other, "Document failed to render")
  }
}

impl<'a, 'b, W: Write> pretty::RenderAnnotated<'b, Annot> for HtmlPrinter<'a, W> {
  /// Push the close annotation's close tag to the stack for later
  /// and write the open tag representation to the underlying writer
  fn push_annotation(&mut self, ann: &'b Annot) -> io::Result<()> {
    macro_rules! tag {
      (span $cls:expr) => {tag!("span", concat!(" class=\"", $cls, "\""))};
      ($tag:expr, $attr:expr) => {{
        write!(self.w.0, concat!("<", $tag, $attr, ">"))?;
        self.stack.push($tag);
      }};
    }
    match *ann {
      Annot::SortModifiers(_) => tag!(span "sortmod"),
      Annot::Visibility(_) => tag!(span "vis"),
      Annot::Keyword => tag!(span "kw"),
      Annot::Prec => tag!(span "prec"),
      Annot::SortName(sid) => {
        write!(self.w.0, "<a class=\"sortname\" href=\"{}index.html#", self.rel)?;
        let ad = &self.env.data[self.env.sorts[sid].atom];
        disambiguated_anchor(&mut self.w.0, ad, false)?;
        write!(self.w.0, "\">")?;
        self.stack.push("a");
      }
      Annot::TermName(tid) => {
        let kind = match &self.env.terms[tid].kind {
          crate::elab::environment::TermKind::Term => "term",
          crate::elab::environment::TermKind::Def(_) => "def"
        };
        write!(self.w.0, "<a class=\"{}\" href=\"{}index.html#", kind, self.rel)?;
        let ad = &self.env.data[self.env.terms[tid].atom];
        disambiguated_anchor(&mut self.w.0, ad, false)?;
        write!(self.w.0, "\">")?;
        self.stack.push("a");
      }
      Annot::ThmName(tid) => {
        let w = &mut *self.w.0;
        let rel = self.rel;
        let kind = match &self.env.thms[tid].kind {
          ThmKind::Axiom => "ax",
          ThmKind::Thm(_) => "thm"
        };
        self.mangler.mangle(self.env, tid, |_, mangled|
          write!(w, r#"<a class="{}" href="{}thms/{}.html">"#, kind, rel, mangled))?;
        self.stack.push("a");
      }
    }
    Ok(())
  }

  /// Used when this annotation's scope closes, so we just pop the close
  /// tag off the stack and write it to the underlying writer
  fn pop_annotation(&mut self) -> Result<(), Self::Error> {
    write!(self.w.0, "</{}>", self.stack.pop().expect("stack underflow"))
  }
}

struct AxiomUse(HashMap<ThmId, BitSet>);

impl AxiomUse {
  fn new(env: &Environment) -> (Vec<ThmId>, Self) {
    let mut axuse = HashMap::new();
    let mut to_tid = vec![ThmId(u32::MAX)];
    for (tid, td) in env.thms.enum_iter() {
      if let ThmKind::Axiom = td.kind {
        let axid = to_tid.len();
        to_tid.push(tid);
        let mut bs = BitSet::new();
        bs.insert(axid);
        axuse.insert(tid, bs);
      }
    }
    (to_tid, AxiomUse(axuse))
  }

  fn accumulate(&mut self, env: &Environment, bs: &mut BitSet, node: &ProofNode) {
    match node {
      ProofNode::Ref(_) |
      ProofNode::Dummy(_, _) |
      ProofNode::Term {..} |
      ProofNode::Hyp(_, _) |
      ProofNode::Refl(_) |
      ProofNode::Sym(_) |
      ProofNode::Cong {..} |
      ProofNode::Unfold {..} => {}
      ProofNode::Conv(p) => self.accumulate(env, bs, &p.2),
      &ProofNode::Thm {thm: tid, ref args, ..} => {
        bs.union_with(self.get(env, tid));
        for p in &**args { self.accumulate(env, bs, p) }
      }
    }
  }

  fn get<'a, 'b>(&'a mut self, env: &'b Environment, tid: ThmId) -> &'a BitSet {
    if let Some(bs) = self.0.get(&tid) {
      // Safety: This is the same issue that comes up in Spans::insert.
      // We are performing a lifetime cast here because rust can't see that
      // in the None case it is safe to drop the borrow of `self.axuse`.
      #[allow(clippy::useless_transmute, clippy::transmute_ptr_to_ptr)]
      return unsafe { std::mem::transmute(bs) }
    }
    let mut bs = BitSet::new();
    let td = &env.thms[tid];
    match &td.kind {
      ThmKind::Axiom => unreachable!(),
      ThmKind::Thm(None) => {bs.insert(0);}
      ThmKind::Thm(Some(Proof {heap, head, ..})) => {
        for p in &heap[td.args.len()..] { self.accumulate(env, &mut bs, p) }
        self.accumulate(env, &mut bs, head)
      }
    }
    self.0.entry(tid).or_insert(bs)
  }
}

#[derive(Debug, Clone)]
enum LineKind {
  Hyp(Option<AtomId>),
  Thm(ThmId),
  Conv(Box<[TermId]>),
}
struct Line {
  hyps: Box<[u32]>,
  kind: LineKind,
  expr: LispVal,
}

struct LayoutProof<'a> {
  env: &'a mut Environment,
  lines: Vec<Line>,
  rev: bool,
  args: &'a [(Option<AtomId>, Type)],
  heap: &'a [ProofNode],
  hyps: &'a [(Option<AtomId>, ExprNode)],
  heap_lines: Box<[Option<LayoutResult>]>,
}

#[derive(Clone)]
enum LayoutResult {
  Expr(LispVal),
  Proof(u32),
  Conv(Box<[TermId]>),
}

impl LayoutResult {
  fn into_expr(self) -> LispVal {
    if let LayoutResult::Expr(e) = self {e} else {panic!("bad proof")}
  }
  fn into_proof(self) -> u32 {
    if let LayoutResult::Proof(e) = self {e} else {panic!("bad proof")}
  }
  fn into_conv(self) -> Box<[TermId]> {
    if let LayoutResult::Conv(e) = self {e} else {panic!("bad proof")}
  }
  fn as_conv(&self) -> &[TermId] {
    if let LayoutResult::Conv(e) = self {e} else {panic!("bad proof")}
  }
}

impl<'a> LayoutProof<'a> {
  fn push_line(&mut self, hyps: Box<[u32]>, kind: LineKind, expr: LispVal) -> u32 {
    let line = self.lines.len().try_into().expect("lines are u32");
    self.lines.push(Line {hyps, kind, expr});
    line
  }

  fn layout_conv(&mut self, defs: &mut Vec<TermId>, p: &ProofNode) {
    match p {
      &ProofNode::Ref(i) => {
        for &d in
          if let Some(h) = &self.heap_lines[i] {h} else {
            let h = self.layout(&self.heap[i]);
            self.heap_lines[i].get_or_insert(h)
          }.as_conv()
        { if !defs.contains(&d) {defs.push(d)} }
      },
      ProofNode::Dummy(_, _) |
      ProofNode::Term {..} |
      ProofNode::Hyp(_, _) |
      ProofNode::Thm {..} |
      ProofNode::Conv(_) => unreachable!(),
      ProofNode::Refl(_) => {}
      ProofNode::Sym(c) => self.layout_conv(defs, c),
      ProofNode::Cong {args, ..} => for c in &**args { self.layout_conv(defs, c) },
      &ProofNode::Unfold {term, ref res, ..} => {
        if !defs.contains(&term) {defs.push(term)}
        self.layout_conv(defs, &res.1)
      }
    }
  }

  fn layout(&mut self, p: &ProofNode) -> LayoutResult {
    match *p {
      ProofNode::Ref(i) =>
        if let Some(n) = &self.heap_lines[i] {n.clone()}
        else {
          let res = if let Some((a, _)) = self.args.get(i) {
            let a = a.unwrap_or_else(|| self.env.get_atom(format!("v{}", i+1).as_bytes()));
            LayoutResult::Expr(LispVal::atom(a))
          } else { self.layout(&self.heap[i]) };
          self.heap_lines[i] = Some(res.clone());
          res
        },
      ProofNode::Dummy(a, _) => LayoutResult::Expr(LispVal::atom(a)),
      ProofNode::Term {term, ref args} => {
        let mut out = Vec::with_capacity(args.len()+1);
        out.push(LispVal::atom(self.env.terms[term].atom));
        for e in &**args {
          out.push(self.layout(e).into_expr());
        }
        LayoutResult::Expr(LispVal::list(out))
      }
      ProofNode::Hyp(i, ref e) => {
        let e = self.layout(e).into_expr();
        LayoutResult::Proof(self.push_line(Box::new([]), LineKind::Hyp(self.hyps[i].0), e))
      }
      ProofNode::Thm {thm, ref args, ref res} => {
        let td = &self.env.thms[thm];
        let mut hyps = SliceUninit::new(td.hyps.len());
        if self.rev {
          for (i, e) in args[td.args.len()..].iter().enumerate().rev() {
            hyps.set(i, self.layout(e).into_proof())
          }
        } else {
          for (i, e) in args[td.args.len()..].iter().enumerate() {
            hyps.set(i, self.layout(e).into_proof())
          }
        }
        let res = self.layout(res).into_expr();
        LayoutResult::Proof(self.push_line(unsafe {hyps.assume_init()}, LineKind::Thm(thm), res))
      }
      ProofNode::Conv(ref p) => {
        let n = self.layout(&p.2).into_proof();
        let mut defs = self.layout(&p.1).into_conv();
        let tgt = self.layout(&p.0).into_expr();
        defs.sort_by_key(|&t| self.env.data[self.env.terms[t].atom].name.as_str());
        LayoutResult::Proof(self.push_line(Box::new([n]), LineKind::Conv(defs), tgt))
      }
      ProofNode::Refl(_) |
      ProofNode::Sym(_) |
      ProofNode::Cong {..} |
      ProofNode::Unfold {..} => {
        let mut defs = vec![];
        self.layout_conv(&mut defs, p);
        LayoutResult::Conv(defs.into_boxed_slice())
      }
    }
  }
}

fn render_line<'a>(fe: FormatEnv<'_>, mangler: &'a mut Mangler, w: &mut impl Write,
  line: u32, hyps: &[u32], kind: LineKind, e: &LispVal) -> io::Result<()> {
  let kind_class = match kind {
    LineKind::Hyp(_) => "step-hyp",
    LineKind::Thm(_) => "step-thm",
    LineKind::Conv(_) => "step-conv"
  };
  write!(w, "        \
              <tr id=\"{line}\" class=\"{kind}\">\
    \n          <td>{line}</td>\
    \n          <td>",
    kind = kind_class, line = line)?;
  let mut first = true;
  for hyp in hyps {
    if !mem::take(&mut first) { write!(w, ", ")? }
    write!(w, r##"<a href="#{id}">{id}</a>"##, id = hyp)?
  }
  write!(w, "</td>\n          <td>")?;
  match kind {
    LineKind::Hyp(None) => write!(w, "<i>hyp</i>")?,
    LineKind::Hyp(Some(a)) => write!(w, "<i>hyp {}</i>", fe.env.data[a].name)?,
    LineKind::Thm(tid) =>
      mangler.mangle(fe.env, tid, |thm, mangled|
        write!(w, r#"<a class="{}" href="{}.html">{}</a>"#,
          if let ThmKind::Axiom = fe.env.thms[tid].kind {"ax"} else {"thm"},
          mangled, thm))?,
    LineKind::Conv(defs) => {
      write!(w, "<i>conv</i>")?;
      let mut first = true;
      for &def in &*defs {
        if !mem::take(&mut first) { write!(w, ",")? }
        write!(w, " <a class=\"def\" href=\"../index.html#")?;
        let ad = &fe.env.data[fe.env.terms[def].atom];
        disambiguated_anchor(w, ad, false)?;
        write!(w, "\">{}</a>", ad.name)?;
      }
    }
  }
  write!(w, "</td>\
    \n          <td><pre>")?;
  fe.pretty(|pr| pr.pp_expr(e).1.doc
    .render_raw(PP_WIDTH, &mut HtmlPrinter::new(fe.env, mangler, w, "../"))
    .expect("writing to a string"));
  writeln!(w, "</pre></td>\
    \n        </tr>")
}

#[allow(clippy::too_many_arguments)]
fn render_proof<'a>(
  source: &'a LinedString, env: &'a mut Environment, mangler: &'a mut Mangler, w: &mut impl Write,
  order: ProofOrder,
  args: &'a [(Option<AtomId>, Type)],
  hyps: &'a [(Option<AtomId>, ExprNode)],
  pf: &'a Proof,
) -> io::Result<()> {
  let mut layout = LayoutProof {
    env, args, heap: &pf.heap, hyps,
    rev: matches!(order, ProofOrder::Pre), lines: vec![],
    heap_lines: vec![None; pf.heap.len()].into_boxed_slice()
  };
  layout.layout(&pf.head).into_proof();
  let lines = layout.lines;
  let fe = FormatEnv {source, env};
  match order {
    ProofOrder::Post =>
      for (line, Line {mut hyps, kind, expr}) in lines.into_iter().enumerate() {
        for i in &mut *hyps { *i += 1 }
        render_line(fe, mangler, w, (line + 1).try_into().expect("lines are u32"), &hyps, kind, &expr)?;
      }
    ProofOrder::Pre => {
      let len = lines.len();
      for (line, Line {mut hyps, kind, expr}) in lines.into_iter().rev().enumerate() {
        for i in &mut *hyps { *i = u32::try_from(len).expect("lines are u32") - *i }
        render_line(fe, mangler, w, (line + 1).try_into().expect("lines are u32"), &hyps, kind, &expr)?;
      }
    }
  }
  Ok(())
}

#[repr(transparent)]
struct CaseInsensitiveName(ArcString);

impl<'a> From<&'a ArcString> for &'a CaseInsensitiveName {
  #[allow(clippy::transmute_ptr_to_ptr)]
  fn from(s: &'a ArcString) -> Self { unsafe { mem::transmute(s) } }
}
impl Hash for CaseInsensitiveName {
  fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
    self.0.len().hash(state);
    for &c in &*self.0 {
      c.to_ascii_lowercase().hash(state)
    }
  }
}
impl PartialEq for CaseInsensitiveName {
  fn eq(&self, other: &Self) -> bool {
    self.0.len() == other.0.len() &&
    self.0.iter().zip(other.0.iter()).all(|(&x, &y)|
      x.to_ascii_lowercase() == y.to_ascii_lowercase())
  }
}
impl Eq for CaseInsensitiveName {}

#[derive(Clone, Copy)]
enum ProofOrder { Pre, Post }

struct BuildDoc<'a, W> {
  thm_folder: PathBuf,
  source: &'a LinedString,
  base_url: Option<Url>,
  env: Environment,
  axuse: (Vec<ThmId>, AxiomUse),
  index: Option<W>,
  mangler: Mangler,
  order: ProofOrder,
}

#[derive(Default)]
struct Mangler {
  used: HashMap<CaseInsensitiveName, usize>,
  thms: HashMap<ThmId, usize>,
}
impl Mangler {
  fn get(&mut self, env: &Environment, tid: ThmId) -> usize {
    match self.thms.entry(tid) {
      Entry::Occupied(e) => *e.get(),
      Entry::Vacant(e) => {
        let n = match self.used.entry(CaseInsensitiveName(
          env.data[env.thms[tid].atom].name.clone()
        )) {
          Entry::Occupied(mut e) => {
            let p = e.get_mut();
            let n = *p + 1;
            *p = n;
            n
          }
          Entry::Vacant(e) => *e.insert(0),
        };
        *e.insert(n)
      }
    }
  }
  fn mangle<T>(&mut self, env: &Environment, tid: ThmId, f: impl FnOnce(&str, &str) -> T) -> T {
    let s = env.data[env.thms[tid].atom].name.as_str();
    match self.get(env, tid) {
      0 => f(s, s),
      n => f(s, &format!("{}.{}", s, n))
    }
  }
}

fn header(w: &mut impl Write,
  rel: &str, desc: &str, title: &str,
  h1: &str, nav: &str, script: &[&str]
) -> io::Result<()> {
  writeln!(w, "\
      <!DOCTYPE html>\
    \n<html lang=\"en\">\
    \n<head>\
    \n  <meta charset=\"utf-8\">\
    \n  <meta name=\"viewport\" content=\"width=device-width, initial-scale=1.0\">\
    \n  <meta name=\"generator\" content=\"mm0-doc\">\
    \n  <meta name=\"description\" content=\"{desc}\">\
    \n  <meta name=\"keywords\" content=\"mm0, metamath-zero\">\
    \n  <title>{title} - Metamath Zero</title>\
    \n  <link rel=\"stylesheet\" type=\"text/css\" href=\"{rel}stylesheet.css\" />\
    \n  <link rel=\"stylesheet\" href=\"https://fonts.googleapis.com/css?family=Neuton&amp;subset=latin\" type=\"text/css\" media=\"screen\">\
    \n  <link rel=\"stylesheet\" href=\"https://fonts.googleapis.com/css?family=Nobile:regular,italic,bold,bolditalic&amp;subset=latin\" type=\"text/css\" media=\"screen\">",
    rel = rel, desc = desc, title = title)?;
  for s in script {
    writeln!(w, r#"  <script src="{}"></script>"#, s)?
  }
  writeln!(w, "  \
        <!-- <link rel=\"shortcut icon\" href=\"{rel}favicon.ico\"> -->\
    \n</head>\
    \n<body>\
    \n  <div class=\"body\">\
    \n    <h1 class=\"title\">\
    \n      {h1}\
    \n      <span class=\"nav\">{nav}</span>\
    \n    </h1>",
    rel = rel, h1 = h1, nav = nav)
}
const FOOTER: &str = "  </div>\n</body>\n</html>";

fn render_doc(w: &mut impl Write, doc: &Option<DocComment>) -> io::Result<()> {
  if let Some(doc) = doc {
    use pulldown_cmark::{Parser, html};
    write!(w, r#"      <div class="doc">"#)?;
    html::write_html(&mut *w, Parser::new(doc))?;
    writeln!(w, "</div>")?;
  }
  Ok(())
}

fn disambiguated_anchor(w: &mut impl Write, ad: &AtomData, sort: bool) -> io::Result<()> {
  match ad {
    AtomData {sort: Some(_), decl: Some(_), ..} if sort => write!(w, "{}.sort", ad.name),
    AtomData {sort: Some(_), decl: Some(DeclKey::Term(_)), ..} => write!(w, "{}.term", ad.name),
    AtomData {sort: Some(_), decl: Some(DeclKey::Thm(_)), ..} => write!(w, "{}.thm", ad.name),
    _ => write!(w, "{}", ad.name),
  }
}

impl<'a, W: Write> BuildDoc<'a, W> {
  fn thm_doc(&mut self,
    prev: Option<ThmId>, tid: ThmId, next: Option<ThmId>
  ) -> io::Result<std::path::PathBuf> {
    let mut path = self.thm_folder.clone();
    #[allow(clippy::useless_transmute)]
    let td: &Thm = unsafe { mem::transmute(&self.env.thms[tid]) };
    self.mangler.mangle(&self.env, tid, |_, s| path.push(&format!("{}.html", s)));
    let mut file = BufWriter::new(File::create(&path)?);
    let ad = &self.env.data[td.atom];
    let thmname = &ad.name;
    let filename = td.span.file.rel();
    let mut nav = String::new();
    if let Some(prev) = prev {
      use std::fmt::Write;
      self.mangler.mangle(&self.env, prev, |thm, mangled|
        write!(nav, r#"<a href="{}.html" title="{}">&#8810;</a> | "#, mangled, thm)
          .expect("writing to a string"));
    }
    nav.push_str("<a href=\"../index.html#");
    disambiguated_anchor(unsafe {nav.as_mut_vec()}, ad, true)?;
    nav.push_str("\">index</a>");
    if let Some(base) = &self.base_url {
      use std::fmt::Write;
      let url = base.join(filename).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
      write!(nav, " | <a href=\"{}", url).expect("writing to a string");
      let range = self.source.to_range(td.full);
      if range.start.line == range.end.line {
        write!(nav, "#L{}\">src</a>", range.start.line + 1)
      } else {
        write!(nav, "#L{}-L{}\">src</a>", range.start.line + 1, range.end.line + 1)
      }.expect("writing to a string");
    }
    if let Some(next) = next {
      use std::fmt::Write;
      self.mangler.mangle(&self.env, next, |thm, mangled|
        write!(nav, r#" | <a href="{}.html" title="{}">&#8811;</a>"#, mangled, thm)
          .expect("writing to a string"));
    }
    let (kind, kindclass) = if let ThmKind::Axiom = td.kind {("Axiom", "ax")} else {("Theorem", "thm")};
    header(&mut file, "../",
      &format!("Documentation for theorem `{}` in `{}`.", thmname, filename),
      &format!("{} - {}", thmname, filename),
      &format!(r#"{} <a class="{}" href="">{}</a>"#, kind, kindclass, thmname),
      &nav, &["../proof.js"])?;
    render_doc(&mut file, &td.doc)?;
    writeln!(file, "    <pre>{}</pre>", FormatEnv {source: self.source, env: &self.env}.to(td))?;
    if let ThmKind::Thm(Some(pf)) = &td.kind {
      writeln!(file, "    \
              <table class=\"proof\">\
        \n      <tbody>\
        \n        <tr class=\"proof-head\">\
                    <th>Step</th><th>Hyp</th><th>Ref</th><th>Expression</th>\
                  </tr>")?;
      render_proof(self.source, &mut self.env, &mut self.mangler,
        &mut file, self.order, &td.args, &td.hyps, pf)?;
      writeln!(file, "      </tbody>\n    </table>")?;
    }
    if let ThmKind::Thm(_) = td.kind {
      writeln!(file, "    <h2 class=\"axioms\">Axiom use</h2>")?;
      let mut first = true;
      for i in self.axuse.1.get(&self.env, tid) {
        if !mem::take(&mut first) { writeln!(file, ",")? }
        if i == 0 {
          write!(file, "<i>sorry</i>")?
        } else {
          self.mangler.mangle(&self.env, self.axuse.0[i], |thm, mangled|
            write!(file, r#"    <a class="ax" href="{}.html">{}</a>"#, mangled, thm))?
        }
      }
      writeln!(file)?
    }
    writeln!(file, "{}", FOOTER)?;
    Ok(path)
  }

  fn write_all(&mut self,
    path: &FileRef, to_open: Option<ThmId>, stmts: &[StmtTrace]
  ) -> io::Result<Option<std::path::PathBuf>> {
    let file = self.index.as_mut().expect("index file missing");
    let nav: String;
    let nav = if let Some(base) = &self.base_url {
      let url = base.join(path.rel()).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
      nav = format!("<a href=\"{}\">src</a>", url);
      &nav
    } else {""};
    header(file, "",
      &format!("Documentation index for `{}`.", path.rel()),
      &format!("{} - Index", path.rel()),
      "Index", nav, &[])?;
    let mut prev = None;
    let mut open_path = None;
    for s in stmts {
      let mut file = self.index.as_mut().expect("index file missing");
      let fe = FormatEnv {source: self.source, env: &self.env};
      match *s {
        StmtTrace::Global(_) |
        StmtTrace::OutputString(_) => {}
        StmtTrace::Sort(a) => {
          let ad = &self.env.data[a];
          write!(file, "    <div id=\"")?;
          disambiguated_anchor(&mut file, ad, true)?;
          writeln!(file, "\">")?;
          let sid = ad.sort.expect("wf env");
          let sd = &self.env.sorts[sid];
          render_doc(&mut file, &sd.doc)?;
          writeln!(file, "      <pre>")?;
          let w = &mut HtmlPrinter::new(fe.env, &mut self.mangler, file, "");
          fe.pretty(|pr| pr.sort(sid).render_raw(PP_WIDTH, w))?;
          writeln!(file, "</pre>\n    </div>")?
        }
        StmtTrace::Decl(a) => {
          let ad = &self.env.data[a];
          write!(file, "    <div id=\"")?;
          disambiguated_anchor(&mut file, ad, false)?;
          writeln!(file, "\">")?;
          match ad.decl.expect("wf env") {
            DeclKey::Term(tid) => {
              let td = &self.env.terms[tid];
              render_doc(&mut file, &td.doc)?;
              write!(file, "      <pre>")?;
              let w = &mut HtmlPrinter::new(fe.env, &mut self.mangler, file, "");
              fe.pretty(|pr| pr.term_and_notations(tid, true).render_raw(PP_WIDTH, w))?;
              writeln!(file, "</pre>\n    </div>")?
            }
            DeclKey::Thm(tid) => {
              let td = &self.env.thms[tid];
              render_doc(&mut file, &td.doc)?;
              write!(file, "      <pre>")?;
              let w = &mut HtmlPrinter::new(fe.env, &mut self.mangler, file, "");
              fe.pretty(|pr| pr.thm(tid).render_raw(PP_WIDTH, w))?;
              writeln!(file, "</pre>\n    </div>")?;
              let next = tid.0.checked_add(1).map(ThmId)
                .filter(|&tid| self.env.thms.get(tid).is_some());
              let path = self.thm_doc(prev, tid, next)?;
              if to_open == Some(tid) { open_path = Some(path) }
              prev = Some(tid);
            }
          }
        }
      }
    }
    let file = self.index.as_mut().expect("index file missing");
    writeln!(file, "{}", FOOTER)?;
    Ok(open_path)
  }
}
/// Main entry point for `mm0-rs doc` subcommand.
///
/// # Arguments
///
/// `mm0-rs doc <in.mm1> [doc]`, where:
///
/// - `in.mm1` is the initial file to elaborate.
/// - `doc` is the output folder, which will be created if not present.
pub fn main(args: &ArgMatches<'_>) -> io::Result<()> {
  let path = args.value_of("INPUT").expect("required arg");
  let path: FileRef = fs::canonicalize(path)?.into();
  let (fc, old) = crate::compiler::elab_for_result(path.clone())?;
  let old = old.unwrap_or_else(|| std::process::exit(1));
  println!("writing docs");
  let mut env = Environment::new();
  assert!(matches!(
    EnvMergeIter::new(&mut env, &old, (0..0).into()).next(&mut env, &mut vec![]), Ok(None)));
  let mut dir = PathBuf::from(args.value_of("OUTPUT").unwrap_or("doc"));
  fs::create_dir_all(&dir)?;
  macro_rules! import {($($str:expr),*) => {$({
    let mut file = dir.to_owned();
    file.push($str);
    if !file.exists() {
      File::create(file)?.write_all(include_bytes!($str))?;
    }
  })*}}
  import!("stylesheet.css", "proof.js");
  let order = match args.value_of("order") {
    Some("pre") => ProofOrder::Pre,
    Some("post") => ProofOrder::Post,
    _ => unreachable!(),
  };
  let mut to_open = None;
  let only = args.values_of("only");
  let index = if only.is_some() {None} else {
    let mut path = dir.clone();
    path.push("index.html");
    let file = File::create(&path)?;
    to_open = Some(path);
    Some(BufWriter::new(file))
  };
  dir.push("thms");
  fs::create_dir_all(&dir)?;
  let base_url = match args.value_of("src") {
    Some("-") => None,
    src => Some(Url::parse(src.unwrap_or("https://github.com/digama0/mm0/blob/master/examples/"))
      .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?),
  };
  let mut bd = BuildDoc {
    source: fc.ascii(),
    base_url, order,
    axuse: AxiomUse::new(&env),
    thm_folder: dir, env, index,
    mangler: Mangler::default(),
  };
  let mut get_thm = |thm: &str| {
    let a = bd.env.get_atom(thm.as_bytes());
    match bd.env.data[a].decl {
      None => eprintln!("warning: unknown theorem '{}'", thm),
      Some(DeclKey::Term(_)) => eprintln!("warning: expected a theorem, got term '{}'", thm),
      Some(DeclKey::Thm(tid)) => return Some(tid),
    }
    None
  };
  let target = args.value_of("open").and_then(&mut get_thm);
  if let Some(only) = only {
    let thms = only.filter_map(get_thm).collect::<Vec<_>>();
    for (i, &tid) in thms.iter().enumerate() {
      let path = bd.thm_doc(i.checked_sub(1).map(|j| thms[j]), tid, thms.get(i+1).copied())?;
      if to_open.is_none() || target == Some(tid) {
        to_open = Some(path)
      }
    }
  } else if let Some(path) = bd.write_all(&path, target, old.stmts())? {
    to_open = Some(path)
  }
  if args.is_present("open") || args.is_present("open_to") {
    if let Some(path) = to_open {
      let path = std::fs::canonicalize(path).expect("bad path");
      let path = Url::from_file_path(path).expect("bad path");
      drop(webbrowser::open(path.as_str()))
    } else {
      eprintln!("warning: --open specified but no pages generated")
    }
  }
  Ok(())
}
