/* One log model, independent of transport and DOM. */
(function (root, factory) {
  const api = factory();
  if (typeof module === 'object') module.exports = api;
  else root.ReveLog = api;
})(globalThis, function () {
  'use strict';
  function text(content) {
    if (typeof content === 'string') return content;
    if (!Array.isArray(content)) return '';
    return content.filter(p => p && p.type === 'text').map(p => p.text || '').join('');
  }
  class Log {
    constructor() { this.entries = new Map(); }
    upsert(record) {
      if (!record || !record.entry || !record.entry.id) return false;
      const id = record.entry.id, previous = this.entries.get(id);
      if (previous && previous.status === 'committed' && record.status !== 'committed') return false;
      if (previous && previous.status === 'streaming' && record.status === 'streaming' &&
          (previous.revision || 0) > (record.revision || 0)) return false;
      const next = structuredClone(record);
      const message = next.entry.message || {};
      if (previous && message.role === 'assistant' && !text(message.content) &&
          (message.stopReason === 'error' || message.stopReason === 'aborted')) {
        const partial = previous.draftText || (previous.status !== 'committed' ? text((previous.entry.message || {}).content) : '');
        if (partial) next.draftText = partial; // explicitly memory-only, never a durable answer
      }
      if (previous && JSON.stringify(previous) === JSON.stringify(next)) return false;
      this.entries.set(id, next); return true;
    }
    merge(records) { for (const record of records) this.upsert(record); }
    event(event) {
      if (event.type === 'entry_added') return this.upsert({entry:event.entry, status:'committed', order:event.entry.seq, revision:0});
      if (event.type === 'entry_draft') return this.upsert({entry:event.entry, status:'streaming', order:event.order, revision:event.version});
      if (event.type === 'entry_accepted') return this.upsert({entry:event.entry, status:'accepted', order:event.order, revision:0});
      if (event.type === 'run_abort') {
        for (const record of this.entries.values()) {
          if (record.status === 'accepted' && record.entry.message && record.entry.message.role === 'user' &&
              record.entry.display && record.entry.display.run_id === event.run_id) record.status = 'cancelled';
        }
        return true;
      }
      if (event.type === 'run_end' || event.type === 'fault') { this.interrupt(event.run_id); return true; }
      return false;
    }
    interrupt(run) {
      for (const record of this.entries.values()) {
        if (record.status === 'streaming' && (!run || (record.entry.display || {}).run_id === run)) {
          record.status = 'interrupted'; record.revision = 0;
        }
      }
    }
    records() { return [...this.entries.values()].sort((a,b) => a.order - b.order || a.entry.id.localeCompare(b.entry.id)); }
  }
  function project(records) {
    const rows = [];
    for (const record of records) {
      const e = record.entry, m = e.message || {}, display = e.display || {};
      const base = {id:e.id, run:display.run_id || '', status:record.status};
      if (e.type === 'custom' && e.customType === 'user_notice') {
        if (e.data && e.data.text) rows.push({...base, kind:'assistant', text:e.data.text});
      } else if (e.type === 'compaction') {
        rows.push({...base, kind:'marker', text:'Context compacted'});
      } else if (m.role === 'user') {
        rows.push({...base, kind:'user', text:text(m.content), from:m.from_bot, fromName:m.from_name});
      } else if (m.role === 'assistant') {
        const failed = m.stopReason === 'error' || m.stopReason === 'aborted';
        if (display.audience === 'internal') {
          if (failed && m.errorMessage) rows.push({...base, kind:'notice', text:m.errorMessage});
          continue;
        }
        const body = record.draftText || text(m.content);
        if (body || (failed && m.errorMessage)) rows.push({...base, kind:'assistant', text:body || m.errorMessage,
          label:record.draftText ? 'Interrupted draft · not persisted' : failed ? (m.stopReason === 'aborted' ? 'Interrupted' : 'Request failed') : record.status === 'interrupted' ? 'Interrupted draft · not persisted' : ''});
        // Tool-call arguments are intent, never a user-visible message or a result.
      } else if (m.role === 'toolResult') {
        const name = display.name || m.toolName || 'tool';
        const failed = !!m.isError;
        if (name === 'SendUserMessage' && !failed) continue;
        rows.push({...base, kind:name === 'AskUserForSecret' && record.status === 'streaming' ? 'secret' : 'tool',
          name, args:display.args || {}, text:text(m.content), failed, synthetic:m.synthetic || null,
          uncertain:record.status === 'interrupted', running:record.status === 'streaming'});
      }
    }
    return rows;
  }
  function group(rows) {
    const grouped = [], activities = new Map();
    for (const row of rows) {
      if (row.kind === 'tool') {
        const key = row.run || row.id;
        if (activities.has(key)) activities.get(key).tools.push(row);
        else {
          const activity = {kind:'activity', id:'activity:' + key, run:row.run, tools:[row]};
          activities.set(key, activity); grouped.push(activity);
        }
      } else grouped.push(row);
    }
    return grouped;
  }
  function action(tool) {
    if (typeof tool.args.description === 'string' && tool.args.description) return tool.args.description;
    const labels = {read:'Reading files', ls:'Exploring folders', glob:'Finding files', grep:'Searching the workspace', bash:'Running a command', write:'Writing files', edit:'Editing files', cd:'Changing directory'};
    return labels[tool.name] || tool.name;
  }
  function summary(tools) {
    const active = tools.filter(t => t.running).at(-1);
    const failed = tools.filter(t => t.failed).length;
    if (active) return action(active) + ' · ' + tools.length + (tools.length === 1 ? ' action' : ' actions');
    const counts = new Map();
    for (const t of tools) if (!t.synthetic && !t.uncertain) counts.set(t.name, (counts.get(t.name) || 0) + 1);
    const labels = {read:['Read','file','files'], ls:['Listed','directory','directories'], bash:['Ran','command','commands'], write:['Wrote','file','files'], edit:['Edited','file','files'], glob:['Ran','file search','file searches'], grep:['Ran','search','searches']};
    const parts = [...counts].map(([name,n]) => labels[name] ? `${labels[name][0]} ${n} ${labels[name][n === 1 ? 1 : 2]}` : `${name} ×${n}`);
    const blocked = tools.filter(t => t.synthetic && t.synthetic !== 'interrupted').length;
    const uncertain = tools.filter(t => t.uncertain || t.synthetic === 'interrupted').length;
    if (blocked) parts.push(`${blocked} blocked or rejected`);
    if (uncertain) parts.push(`${uncertain} awaiting a committed result`);
    if (failed) parts.push(`${failed} failed`);
    return parts.join(' · ');
  }
  return {Log, text, project, group, summary, action};
});
