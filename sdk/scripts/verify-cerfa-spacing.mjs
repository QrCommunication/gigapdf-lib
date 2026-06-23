// Ad-hoc verification of recon gap-aware spacing on a dense multi-font CERFA.
// Flattens pageBlocks() to text and prints the lines that exhibit the spurious-
// space bug (titles + notice prose). Run before AND after the fix.
import { GigaPdfEngine } from '../dist/index.js';
import { readFileSync } from 'node:fs';

const PDF = process.argv[2] || '/home/rony/Téléchargements/pdfs/s3705_puma_version_ameli_remp.pdf';

function inlineText(inls) {
  let s = '';
  for (const i of inls) {
    if (i.t === 'run') s += i.v.text;
    else if (i.t === 'br') s += '\n';
    else if (i.t === 'link') s += inlineText(i.children);
  }
  return s;
}
function blockText(b, out) {
  const k = b.kind;
  if (k.t === 'paragraph') out.push(inlineText(k.v.runs));
  else if (k.t === 'heading') out.push('[H' + k.v.level + '] ' + inlineText(k.v.para.runs));
  else if (k.t === 'list') for (const it of k.v.items) for (const bb of it.blocks) blockText(bb, out);
  else if (k.t === 'table') for (const r of k.v.rows) for (const c of r.cells) for (const bb of c.blocks) blockText(bb, out);
  else if (k.t === 'textbox') for (const bb of k.v.blocks) blockText(bb, out);
}

const giga = await GigaPdfEngine.loadDefault();
const doc = giga.open(new Uint8Array(readFileSync(PDF)));
const n = doc.pageCount();

const all = [];
for (let p = 1; p <= n; p++) {
  const blocks = doc.pageBlocks(p);
  for (const b of blocks) blockText(b, all);
}
const flat = all.map((l) => l.replace(/\n/g, ' ').trim()).filter(Boolean);

// Probe markers: substrings that, broken by spurious spaces, become diagnostic.
const probes = [
  ['ENFANTS MINEURS', /ENFANT\s+S\s+MINEUR\s+S|ENFANTS MINEURS/],
  ['relatif au rattachement', /relat\s*i\s*f|relatif/i],
  ['Articles', /Art\s+icles|Articles/],
  ['MALADIE', /MALAD\s+IE|MALADIE/],
  ['DES ENFANTS (space kept)', /DES\s+ENFANTS|DESENFANTS/],
];
console.log(`pages=${n}  lines=${flat.length}\n`);
for (const [label, re] of probes) {
  const hits = flat.filter((l) => re.test(l)).slice(0, 4);
  console.log(`### ${label}`);
  if (hits.length === 0) console.log('   (no line matched)');
  for (const h of hits) console.log('   | ' + h.slice(0, 160));
  console.log();
}
// Also dump the first heading-ish lines (the title) verbatim.
console.log('### first 12 non-empty lines (title region)');
for (const l of flat.slice(0, 12)) console.log('   | ' + l.slice(0, 160));
