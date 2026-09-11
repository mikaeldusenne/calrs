const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { join } = require('node:path');
const { test } = require('node:test');
const vm = require('node:vm');

const template = readFileSync(process.env.SLOTS_TEMPLATE || join(__dirname, '../templates/slots.html'), 'utf8');
const monthName = new Intl.DateTimeFormat('en', { month: 'long', year: 'numeric' });
const monthString = date => `${date.getFullYear()}-${String(date.getMonth() + 1).padStart(2, '0')}`;
const slot = date => ({ guestDate: date, hostDate: date, start: '09:00', hostTime: '09:00' });
const settle = () => new Promise(resolve => setImmediate(resolve));

function monthData(month, slots = {}, extra = {}) {
  const start = new Date(`${month}-01T12:00:00`);
  return {
    slotData: slots,
    availableDates: Object.fromEntries(Object.keys(slots).map(date => [date, true])),
    monthYear: month,
    monthLabel: monthName.format(start),
    firstWeekday: (start.getDay() + 6) % 7,
    daysInMonth: new Date(start.getFullYear(), start.getMonth() + 1, 0).getDate(),
    todayDate: '2027-02-01',
    prevMonth: monthString(new Date(start.getFullYear(), start.getMonth() - 1, 1)),
    nextMonth: monthString(new Date(start.getFullYear(), start.getMonth() + 1, 1)),
    hasPrevMonth: true,
    hasNextMonth: true,
    ...extra,
  };
}

// Only the DOM operations used by the inline calendar script are needed here.
class Element {
  constructor() {
    this.children = [];
    this.dataset = {};
    this.style = {};
    this.listeners = {};
    this.attributes = {};
    this.className = '';
    this.textContent = '';
    this.classList = {
      contains: name => this.className.split(' ').includes(name),
      add: name => { if (!this.classList.contains(name)) this.className += ` ${name}`; },
      remove: name => { this.className = this.className.split(' ').filter(value => value !== name).join(' '); },
      toggle: (name, active) => this.classList[active ? 'add' : 'remove'](name),
    };
  }
  set innerHTML(html) { this.html = html; this.children = []; this.textContent = html; this.value = html; }
  get innerHTML() { return this.html || ''; }
  appendChild(element) { this.children.push(element); return element; }
  addEventListener(name, listener) { this.listeners[name] = listener; }
  setAttribute(name, value) { this.attributes[name] = value; }
  removeAttribute(name) { delete this.attributes[name]; }
  getAttribute(name) { return this.attributes[name]; }
  querySelectorAll(selector) {
    if (selector.startsWith('.')) {
      return this.children.filter(child => selector.slice(1).split('.').every(name => child.classList.contains(name)));
    }
    return [];
  }
  querySelector(selector) {
    const date = selector.match(/^\[data-date="(.*)"\]$/)?.[1];
    return this.children.find(child => child.dataset.date === date) || null;
  }
  click() { return (this.listeners.click || this.onclick)?.({ preventDefault() {} }); }
}

const response = data => ({ ok: true, status: 200, text: async () => JSON.stringify(data) });

// Execute the production script, including initialization and real click handlers.
// The injected export only exposes state; it does not replace calendar behavior.
function calendar({
  initial = monthData('2027-02'), view = 'week', fetchData, base = '/u/host/meeting',
  search = '?tz=Europe%2FParis', invite = '', embed = '',
} = {}) {
  const elements = Object.fromEntries([
    'calendar-data', 'cal-grid', 'slot-panel-title', 'slot-list', 'main-cal-nav',
    'view-toggle', 'slots-layout', 'week-view', 'column-view', 'cal-header-title',
    'calendar-status', 'calendar-status-text', 'calendar-retry', 'calendar-loading',
  ].map(id => [id, new Element()]));
  elements['calendar-data'].textContent = JSON.stringify(initial);
  const requests = [];
  const locations = [];
  const timers = [];
  const rememberUrl = (_, __, url) => locations.push(url);
  const context = {
    URL, URLSearchParams, console,
    setTimeout: (callback, delay) => timers.push({ callback, delay }),
    localStorage: { getItem: key => key === 'calrs_calendar_view' ? view : null, setItem() {} },
    window: { location: { search, reload() {} }, addEventListener() {} },
    history: { pushState: rememberUrl, replaceState: rememberUrl },
    calrsLoader: { show() {}, hide() {} },
    document: {
      getElementById: id => elements[id] || null,
      createElement: () => new Element(),
      querySelector: selector => elements[selector === '.slots-layout' ? 'slots-layout' : 'cal-header-title'],
      querySelectorAll: () => [],
    },
    DOMParser: class {
      parseFromString(html) {
        return { getElementById: () => html === '<html>Login</html>' ? null : { textContent: html } };
      }
    },
    fetch: url => {
      requests.push(url);
      const month = new URL(url, 'https://calendar.test').searchParams.get('month');
      return Promise.resolve(fetchData ? fetchData(month, url) : response(monthData(month, {}, { todayDate: initial.todayDate })));
    },
  };
  const script = template.match(/<script>\s*([\s\S]*?)<\/script>/)[1]
    .replace(/\{\{([\s\S]*?)\}\}/g, (_, expression) => {
      if (expression.includes('can_book')) return 'true';
      if (expression.trim() === 'base') return base;
      if (expression.trim() === 'guest_tz') return 'Europe/Paris';
      if (expression.trim().startsWith('invite_token')) return invite;
      if (expression.trim().startsWith('embed_qs_amp')) return embed;
      return '';
    })
    .replace(/\}\)\(\);\s*$/, `
      globalThis.calendar = {
        getWeekDates, navigateMonth, switchView,
        state: () => ({ monthYear, slotData, availableDates, currentView })
      };
    })();`);
  vm.runInNewContext(script, context, { filename: 'templates/slots.html' });
  return {
    ...context.calendar, elements, requests, locations, timers,
    tick: async () => { timers.shift().callback(); await settle(); },
    dates: () => Array.from(context.calendar.getWeekDates(), day => day.dateStr),
    next: () => elements['main-cal-nav'].children.at(-1).click(),
    previous: () => elements['main-cal-nav'].children[0].click(),
  };
}

test('weekly navigation stays consecutive across month, DST and year boundaries', async t => {
  for (const [today, expected] of [
    ['2027-02-01', ['2027-02-08', '2027-02-15', '2027-02-22', '2027-03-01']],
    ['2027-03-22', ['2027-03-29', '2027-04-05']],
    ['2027-12-20', ['2027-12-27', '2028-01-03']],
  ]) {
    await t.test(today, async () => {
      const page = calendar({ initial: monthData(today.slice(0, 7), {}, { todayDate: today }) });
      await settle();
      for (const date of expected) {
        page.next();
        await settle();
        assert.equal(page.dates()[0], date);
      }
      page.previous();
      await settle();
      assert.equal(page.dates()[0], expected.at(-2));
    });
  }
});

test('initial week March 29–April 4 loads and displays slots from both months', async () => {
  const page = calendar({
    initial: monthData('2027-03', { '2027-03-29': [slot('2027-03-29')] }, { todayDate: '2027-03-29' }),
    fetchData: month => response(monthData(month, month === '2027-04' ? { '2027-04-01': [slot('2027-04-01')] } : {})),
  });
  await settle();
  assert.deepEqual([page.dates()[0], page.dates()[6]], ['2027-03-29', '2027-04-04']);
  assert.match(page.elements['week-view'].innerHTML, /date=2027-03-29/);
  assert.match(page.elements['week-view'].innerHTML, /date=2027-04-01/);
});

test('failed availability loads are visible and hide stale booking links until retry succeeds', async t => {
  for (const [name, failure] of [
    ['HTTP error', () => ({ ok: false, status: 503, text: async () => JSON.stringify(monthData('2027-03')) })],
    ['missing calendar data', () => ({ ok: true, text: async () => '<html>Login</html>' })],
    ['network error', () => Promise.reject(new Error('offline'))],
  ]) {
    await t.test(name, async () => {
      let fail = true;
      const page = calendar({
        initial: monthData('2027-02', { '2027-02-01': [slot('2027-02-01')] }),
        view: 'month', fetchData: month => fail ? failure() : response(monthData(month)),
      });
      await settle();
      assert.match(page.elements['slot-list'].innerHTML, /date=2027-02-01/);
      page.navigateMonth(2027, 3);
      assert.equal(page.elements['calendar-loading'].classList.contains('is-active'), true);
      assert.equal(page.elements['calendar-status'].classList.contains('calendar-status-loading'), true);
      await settle();
      assert.match(page.elements['calendar-status-text'].textContent, /failed|unable|could not|error/i);
      assert.equal(page.elements['calendar-loading'].classList.contains('is-active'), false);
      assert.equal(page.elements['calendar-status'].classList.contains('calendar-status-loading'), false);
      assert.equal(page.elements['slots-layout'].style.visibility, 'hidden');
      fail = false;
      page.elements['calendar-retry'].click();
      assert.equal(page.elements['calendar-loading'].classList.contains('is-active'), true);
      assert.equal(page.elements['calendar-status'].classList.contains('calendar-status-loading'), true);
      await settle();
      assert.equal(page.elements['calendar-loading'].classList.contains('is-active'), false);
      assert.equal(page.state().monthYear, '2027-03');
      assert.notEqual(page.elements['slots-layout'].style.visibility, 'hidden');
    });
  }
});

test('a late response cannot overwrite newer month navigation', async () => {
  const pending = {};
  const page = calendar({ view: 'month', fetchData: month => new Promise(resolve => { pending[month] = resolve; }) });
  await settle();
  page.navigateMonth(2027, 3);
  page.navigateMonth(2027, 4);
  pending['2027-04'](response(monthData('2027-04')));
  await settle();
  pending['2027-03'](response(monthData('2027-03')));
  await settle();
  assert.equal(page.state().monthYear, '2027-04');
  assert.match(page.elements['cal-header-title'].textContent, /April/);
});

test('an older request finishing cannot stop the current loading indicator', async () => {
  const pending = {};
  const page = calendar({ view: 'month', fetchData: month => new Promise(resolve => { pending[month] = resolve; }) });
  assert.equal(page.elements['calendar-loading'].classList.contains('is-active'), false);
  page.navigateMonth(2027, 3);
  page.navigateMonth(2027, 4);
  pending['2027-03'](response(monthData('2027-03')));
  await settle();
  assert.equal(page.elements['calendar-loading'].classList.contains('is-active'), true);
  assert.equal(page.elements['slots-layout'].getAttribute('aria-busy'), 'true');
  pending['2027-04'](response(monthData('2027-04')));
  await settle();
  assert.equal(page.elements['calendar-loading'].classList.contains('is-active'), false);
  assert.equal(page.elements['slots-layout'].getAttribute('aria-busy'), 'false');
  assert.equal(page.elements['calendar-status'].hidden, true);
});

test('revisiting a week removes slots no longer returned by the server', async () => {
  const page = calendar({ initial: monthData('2027-03', { '2027-03-01': [slot('2027-03-01')] }) });
  await settle();
  page.next();
  await settle();
  page.previous();
  await settle();
  assert.equal(page.dates()[0], '2027-03-01');
  assert.equal(page.state().slotData['2027-03-01'], undefined);
  assert.doesNotMatch(page.elements['week-view'].innerHTML, /date=2027-03-01/);
});

test('dynamic group week requests retain deferred=1 across a month boundary', async () => {
  const page = calendar({
    base: '/u/host+guest/meeting',
    initial: monthData('2027-02', {}, { todayDate: '2027-02-22', deferredLoad: true }),
    invite: 'invite-token',
    embed: '&embed=1&layout=week&theme=dark',
  });
  await settle();
  page.next();
  await settle();
  assert.ok(page.requests.some(url => new URL(url, 'https://calendar.test').searchParams.get('month') === '2027-03'));
  for (const url of page.requests) {
    const params = new URL(url, 'https://calendar.test').searchParams;
    for (const [key, value] of Object.entries({ deferred: '1', tz: 'Europe/Paris', invite: 'invite-token', embed: '1', layout: 'week', theme: 'dark' })) {
      assert.equal(params.get(key), value, `${key} is retained in ${url}`);
    }
  }
});

test('refreshing a saved split-week URL restores the same seven dates', async () => {
  const page = calendar({ initial: monthData('2027-05'), search: '?tz=Europe%2FParis&week=2027-05-31' });
  await settle();
  assert.deepEqual([page.dates()[0], page.dates()[6]], ['2027-05-31', '2027-06-06']);
  assert.equal(new URL(page.locations.at(-1), 'https://calendar.test').searchParams.get('week'), '2027-05-31');
  assert.ok(page.requests.some(url => new URL(url, 'https://calendar.test').searchParams.get('month') === '2027-06'));
});

test('switching to column view and back retains the displayed week', async () => {
  const page = calendar({ initial: monthData('2027-03') });
  await settle();
  page.next();
  await settle();
  assert.equal(page.dates()[0], '2027-03-08');
  page.switchView('column');
  await settle();
  assert.ok(page.elements['column-view'].classList.contains('active'));
  page.switchView('week');
  await settle();
  assert.equal(page.dates()[0], '2027-03-08');
  assert.ok(page.elements['week-view'].classList.contains('active'));
});

test('late initial group loading cannot replace the month the visitor navigated to', async () => {
  const pending = {};
  const page = calendar({
    initial: monthData('2027-02', {}, { deferredLoad: true }), view: 'month',
    fetchData: month => new Promise(resolve => { pending[month] = resolve; }),
  });
  page.navigateMonth(2027, 3);
  pending['2027-03'](response(monthData('2027-03', { '2027-03-01': [slot('2027-03-01')] })));
  await settle();
  pending['2027-02'](response(monthData('2027-02', { '2027-02-01': [slot('2027-02-01')] })));
  await settle();
  assert.equal(page.state().monthYear, '2027-03');
  assert.match(page.elements['slot-list'].innerHTML, /date=2027-03-01/);
  assert.equal(page.state().slotData['2027-02-01'], undefined);
});

test('initial public availability updates automatically when background sync becomes ready', async () => {
  for (const view of ['month', 'week', 'column']) {
    const slots = view === 'column' ? {} : { '2027-02-01': [slot('2027-02-01')] };
    const page = calendar({
      view, initial: monthData('2027-02', {}, { availabilityStatus: 'pending' }),
      fetchData: month => response(monthData(month, slots, { availabilityStatus: 'ready' })),
    });
    await settle();
    assert.equal(page.elements['slots-layout'].style.visibility, 'hidden');
    assert.match(page.elements['calendar-status-text'].textContent, /loading/i);
    assert.equal(page.elements['calendar-status'].classList.contains('calendar-status-loading'), true);
    assert.equal(page.elements['calendar-loading'].classList.contains('is-active'), true);
    assert.equal(page.timers[0].delay, 2000);
    await page.tick();
    assert.notEqual(page.elements['slots-layout'].style.visibility, 'hidden');
    assert.equal(page.elements['calendar-status'].hidden, true);
    assert.equal(page.elements['calendar-loading'].classList.contains('is-active'), false);
    assert.equal(page.timers.length, 0);
    const rendered = page.elements[view === 'month' ? 'slot-list' : `${view}-view`].innerHTML;
    if (view === 'column') assert.match(rendered, /No available times/); // A verified empty result is valid.
    else assert.match(rendered, /date=2027-02-01/);
  }
});

test('unavailable calendars show an explicit error and only retry on request', async () => {
  const page = calendar({ view: 'month', initial: monthData('2027-02', {}, { availabilityStatus: 'unavailable' }) });
  await settle();
  assert.equal(page.elements['slots-layout'].style.visibility, 'hidden');
  assert.match(page.elements['calendar-status-text'].textContent, /could not be verified/i);
  assert.equal(page.elements['calendar-retry'].hidden, false);
  assert.equal(page.elements['calendar-loading'].classList.contains('is-active'), false);
  assert.equal(page.elements['calendar-status'].classList.contains('calendar-status-loading'), false);
  assert.equal(page.timers.length, 0);
  assert.equal(page.requests.length, 0);
  page.elements['calendar-retry'].click();
  await settle();
  assert.equal(page.requests.length, 1);
  assert.equal(page.elements['calendar-status'].hidden, true);
});

test('pending sync polling stops after a bounded wait and offers retry', async () => {
  const pending = monthData('2027-02', {}, { availabilityStatus: 'pending' });
  const page = calendar({ view: 'month', initial: pending, fetchData: () => response(pending) });
  await settle();
  for (let attempts = 0; page.timers.length && attempts < 20; attempts++) await page.tick();
  assert.equal(page.requests.length, 15);
  assert.equal(page.timers.length, 0);
  assert.equal(page.elements['slots-layout'].style.visibility, 'hidden');
  assert.match(page.elements['calendar-status-text'].textContent, /could not be verified/i);
  assert.equal(page.elements['calendar-retry'].hidden, false);
});

test('new navigation cancels an older pending sync poll', async () => {
  const page = calendar({ view: 'month', initial: monthData('2027-02', {}, { availabilityStatus: 'pending' }) });
  await settle();
  assert.equal(page.timers.length, 1);
  page.navigateMonth(2027, 3);
  await settle();
  await page.tick();
  assert.equal(page.state().monthYear, '2027-03');
  assert.equal(page.requests.length, 1);
  assert.equal(page.elements['calendar-status'].hidden, true);
  assert.equal(page.timers.length, 0);
});
