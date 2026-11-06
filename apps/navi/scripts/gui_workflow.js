/* Injected by the private GUI test host; never loaded by the application. */
(async () => {
  const report = {ok: false, steps: []};
  const assert = (condition, message) => {if (!condition) throw new Error(message);};
  /* The chip opens the launch menu; the full dialog is its last entry. */
  function openNewDialogFromChip() {
    document.querySelector('#sessions .session-chip.new').click();
    const entries = [...document.querySelectorAll('#sessions .launch-more')];
    entries.find(entry => entry.textContent.startsWith('More options')).click();
  }
  async function waitFor(condition, message) {
    const deadline = performance.now() + 8000;
    while (!condition()) {
      if (performance.now() > deadline) throw new Error(message);
      await new Promise(resolve => setTimeout(resolve, 50));
    }
  }
  /* Geometry is read from the live page, so the checks run against the real grid. */
  const hits = (a, b) => !!a && !!b
    && Math.min(a.x + a.width, b.x + b.width) - Math.max(a.x, b.x) > 1
    && Math.min(a.y + a.height, b.y + b.height) - Math.max(a.y, b.y) > 1;

  /* `--layout`: the harness session is opened in the dock and the graph drawer is
   * shown, then the shell is measured. A panel pair that overlaps is a defect;
   * the stage is allowed to sit under a drawer by design. */
  async function runLayout(inspect) {
    /* The stage eases into a new cell for about half a second, so a measurement
     * taken mid-flight describes a frame nobody ever sees — the capture that
     * follows the report used to land there, with the art halfway to its cell
     * (BUG-023's off-centre shot). Every measurement waits for the scene to say
     * its lens, dim channel and entrance are done. */
    async function settled() {
      const deadline = performance.now() + 5000;
      let steady = 0, art = null;
      while (performance.now() < deadline) {
        art = inspect().art;
        steady = art && art.settled ? steady + 1 : 0;
        if (steady >= 3) return art;
        await new Promise(resolve => setTimeout(resolve, 50));
      }
      throw new Error(`The stage never settled inside its cell: ${JSON.stringify(art)}`);
    }
    const inside = (a, s, tolerance = 1) => !!a && !!s
      && a.x >= s.x - tolerance && a.y >= s.y - tolerance
      && a.x + a.w <= s.x + s.width + tolerance && a.y + a.h <= s.y + s.height + tolerance;
    const rect = r => r ? `${Math.round(r.x)},${Math.round(r.y)} ${Math.round(r.w ?? r.width)}x${Math.round(r.h ?? r.height)}` : 'none';
    async function measure(label) {
      await settled();
      const view = inspect();
      const state = {...view.layout, label, viewport: view.viewport, dock: view.dock,
        graphOpen: view.graphOpen, phase: view.phase, terminalDisplay: view.terminalDisplay,
        art: view.art, bootOverlay: view.bootOverlay,
        /* `graphRect` is the panel's cell; `graph` is what the panel renders. */
        graphRect: view.layout.graph, graph: view.graph, project: view.project};
      assert(state.overlaps.length === 0, `${label}: panels overlap (${state.overlaps.join(', ')})`);
      assert(state.stage && state.stage.height >= 180, `${label}: the stage is ${state.stage?.height}px tall`);
      assert(state.chat && state.chat.height >= 120, `${label}: the chat is ${state.chat?.height}px tall`);
      /* The panel can sit in the window while what it holds spills out of it: the
       * text box and the status line have to be inside both. */
      const chatBox = document.querySelector('.chat').getBoundingClientRect();
      for (const selector of ['#chat-form', '.chat-meta', '#command', '#speak-toggle']) {
        const box = document.querySelector(selector).getBoundingClientRect();
        assert(box.height > 0 && box.bottom <= chatBox.bottom + 1 && box.bottom <= innerHeight,
          `${label}: ${selector} ends at ${Math.round(box.bottom)}, the chat at ${Math.round(chatBox.bottom)} in a ${innerHeight}px window`);
      }
      /* The art is framed by its own cell, not by the window: the emblem, the halo,
       * the floor and the rings all have to land inside it, or a panel's edge cuts
       * a straight line through the glow (BUG-020/023). */
      assert(state.art && state.art.settled, `${label}: the stage was still moving`);
      assert(inside(state.art, state.stage), `${label}: the art ${rect(state.art)} leaves the stage ${rect(state.stage)}`);
      assert(state.bootOverlay && state.bootOverlay.display === 'none',
        `${label}: the boot overlay is still ${state.bootOverlay?.display}`);
      /* The graph panel: a real cell and, on this empty project, an em dash with
       * the setup state — never a fabricated 0% (BUG-089). */
      if (state.graphOpen) {
        assert(state.graph && state.graph.open === true, `${label}: the graph panel did not render`);
        assert(state.graphRect && state.graphRect.visible, `${label}: the graph has no visible cell`);
        assert(state.graphRect.width >= 300, `${label}: the graph cell is ${state.graphRect.width}px wide`);
        assert(state.graph.label === '—', `${label}: the empty project shows ${state.graph.label}`);
        assert(state.graph.empty === true, `${label}: the "No aspects defined" state is missing`);
        assert(state.graph.as_of && state.graph.as_of.startsWith('as of '),
          `${label}: the panel has no as_of stamp`);
        assert(state.project && state.project.label, `${label}: the status line has no project`);
      }
      return state;
    }
    const chip = () => document.querySelector('#sessions .session-chip:not(.new)');
    await waitFor(() => chip() !== null, 'The session chip did not render');
    const boot = await measure('boot');
    assert(boot.terminalDisplay === 'none', `the terminal was visible at boot: ${boot.terminalDisplay}`);
    chip().click();
    await waitFor(() => inspect().terminalDisplay !== 'none', 'The terminal dock did not open');
    await waitFor(() => document.querySelector('#terminal-host .xterm') !== null, 'xterm did not mount in the dock');
    window.navi.showGraph(true);
    await waitFor(() => inspect().graphOpen === true, 'The graph did not open');
    await new Promise(resolve => setTimeout(resolve, 150));  // one layout turn with both panels open
    const open = await measure('open');
    assert(open.terminalDisplay !== 'none', 'the terminal dock closed itself');
    assert(open.graphOpen === true, 'the graph closed itself');
    assert(open.terminal && open.terminal.visible, 'the terminal dock has no visible cell');
    assert(!hits(open.terminal, open.chat), 'the terminal covers the chat');
    assert(!hits(open.terminal, open.graph), 'the graph covers the terminal');
    /* The splitter: an arrow key moves the dock 16 px, the dock stops at 120 px
     * instead of collapsing, and the stored ratio returns it to the measured size. */
    const bar = document.getElementById('dock-splitter');
    const pane = document.getElementById('terminal-pane');
    const axis = open.dock === 'bottom' ? 'height' : 'width';
    const extent = axis === 'height' ? innerHeight : innerWidth;
    const size = () => Math.round(pane.getBoundingClientRect()[axis]);
    const press = key => bar.dispatchEvent(new KeyboardEvent('keydown', {key, bubbles: true}));
    press(axis === 'height' ? 'ArrowDown' : 'ArrowRight');
    assert(size() === open.terminal[axis] - 16, `the splitter arrow did not resize the dock: ${size()}`);
    press(axis === 'height' ? 'ArrowUp' : 'ArrowLeft');
    assert(size() === open.terminal[axis], `the splitter arrow did not restore the dock: ${size()}`);
    for (let step = 0; step < 60; step++) press(axis === 'height' ? 'ArrowDown' : 'ArrowRight');
    const collapsed = size();
    assert(collapsed === 120, `the dock passed its 120 px floor: ${collapsed}`);
    assert(collapsed <= extent - 120, `the dock passed its ceiling: ${collapsed}`);
    window.navi.setDock('closed');
    localStorage.setItem('navi.dock', JSON.stringify({mode: open.dock,
      ratios: {[open.dock]: open.terminal[axis] / extent}}));
    chip().click();
    await waitFor(() => inspect().terminalDisplay !== 'none' && size() > 120, 'The dock did not reopen at its stored size');
    report.splitter = {axis, opened: open.terminal[axis], collapsed, restored: size()};
    assert(Math.abs(size() - open.terminal[axis]) <= 1, `the stored ratio restored ${size()} instead of ${open.terminal[axis]}`);
    document.getElementById('terminal-close').click();
    await waitFor(() => inspect().terminalDisplay === 'none', 'The terminal dock did not close');
    const closed = await measure('closed');
    assert(closed.terminalDisplay === 'none', `the terminal stayed visible after close: ${closed.terminalDisplay}`);
    assert(closed.graphOpen === true, 'closing the dock also closed the graph');
    chip().click();
    await waitFor(() => inspect().terminalDisplay !== 'none', 'The terminal dock did not reopen');
    const reopened = await measure('reopened');

    /* BUG-090: one click switches the new-session cwd, the graph, the assistant
     * session, and the status line. Alpha is seeded with a scored task, so the
     * panel has to move from "—" to 100% and back without a reload. */
    const first = inspect().project;
    assert(first.options >= 3, `the switcher holds ${first.options} projects`);
    document.querySelector('#project-switcher .project-button').click();
    await waitFor(() => inspect().project.switcher === true, 'the project menu did not open');
    const options = [...document.querySelectorAll('#project-switcher .project-option')];
    assert(options.length === first.options, `the menu lists ${options.length} of ${first.options}`);
    const alpha = options.find(option => option.textContent.includes('Alpha'));
    assert(alpha, 'the Alpha project is not listed');
    alpha.click();
    await waitFor(() => inspect().project.id === alpha.dataset.project, 'the project did not switch');
    await waitFor(() => inspect().graph.label === '100%', `the graph shows ${inspect().graph.label}`);
    const switched = inspect();
    assert(switched.graph.project_id === alpha.dataset.project,
      `the graph still shows ${switched.graph.project_id}`);
    assert(switched.graph.aspects.length === 1, JSON.stringify(switched.graph.aspects));
    assert(switched.graph.aspects[0].value === '100%', JSON.stringify(switched.graph.aspects[0]));
    assert(switched.graph.aspects[0].width === '100%', JSON.stringify(switched.graph.aspects[0]));
    assert(switched.graph.evidence > 0, 'the evidence drawer has no rows for a scored project');
    assert(switched.graph.empty === false, 'the empty state is still shown for a scored project');
    assert(document.getElementById('project-status').textContent.includes('Alpha'),
      `the status line reads ${document.getElementById('project-status').textContent}`);
    /* The new-session dialog defaults to the active project's directory. */
    openNewDialogFromChip();
    await waitFor(() => document.getElementById('new-dialog').open, 'the new dialog did not open');
    assert(document.getElementById('new-cwd').value === switched.project.path,
      `the new-session cwd is ${document.getElementById('new-cwd').value}`);
    document.getElementById('new-cancel').click();
    /* The assistant session follows the project (BUG-090). */
    const scope = inspect().rpcCalls.filter(call => call.op === 'assistant_scope').pop();
    assert(scope && scope.params.project_id === alpha.dataset.project,
      `assistant scope ${JSON.stringify(scope)}`);
    /* Ctrl+1 returns to the first project: "—" and its empty state again. */
    window.dispatchEvent(new KeyboardEvent('keydown', {key: '1', ctrlKey: true}));
    await waitFor(() => inspect().project.id === first.id, 'Ctrl+1 did not switch back');
    await waitFor(() => inspect().graph.label === '—', `the empty project shows ${inspect().graph.label}`);
    assert(inspect().graph.empty === true, 'the empty state did not return');
    assert(inspect().graph.aspects.length === 0, JSON.stringify(inspect().graph.aspects));
    report.switcher = {first: first.id, alpha: alpha.dataset.project,
      label: switched.graph.label, scope: scope.params.project_id};

    /* The empty state offers the one action that starts a real number: adding an
     * aspect through the daemon. It runs on Beta so the first project stays
     * empty for the next size. The daemon outlives each window, so the first
     * size proves the flow and later sizes prove the created aspect still reads
     * as unknown; a new aspect has no evidence, so both stay "—" (BUG-089). */
    document.querySelector('#project-switcher .project-button').click();
    await waitFor(() => inspect().project.switcher === true, 'the project menu did not reopen');
    const beta = [...document.querySelectorAll('#project-switcher .project-option')]
      .find(option => option.textContent.includes('Beta'));
    assert(beta, 'the Beta project is not listed');
    beta.click();
    await waitFor(() => inspect().project.id === beta.dataset.project, 'Beta did not become active');
    await waitFor(() => inspect().graph.project_id === beta.dataset.project,
      'the graph did not follow the switcher to Beta');
    if (inspect().graph.empty === true) {
      const aspectForm = document.querySelector('#graph-region .aspect-form');
      assert(aspectForm, 'the empty state has no aspect setup');
      aspectForm.querySelector('input').value = 'Sessions';
      aspectForm.requestSubmit();
      await waitFor(() => inspect().graph.empty === false, 'the aspect was not added');
    }
    await waitFor(() => inspect().graph.aspects.length === 1, 'the aspect did not render');
    assert(inspect().graph.label === '—', `a new aspect shows ${inspect().graph.label}`);
    assert(inspect().graph.aspects[0].value === '—', JSON.stringify(inspect().graph.aspects));
    assert(inspect().graph.aspects[0].empty === true, JSON.stringify(inspect().graph.aspects));
    report.steps.push('Empty state added an aspect and kept the headline unknown');
    /* Leave the first project active so the next size measures its empty state. */
    window.dispatchEvent(new KeyboardEvent('keydown', {key: '1', ctrlKey: true}));
    await waitFor(() => inspect().project.id === first.id, 'Ctrl+1 did not return to the first project');

    report.layout = {boot, open, closed, reopened};
    report.steps.push(`Dock ${reopened.dock} and the graph measured with no panel overlap`);
    report.ok = true;
  }

  async function runWorkflow(inspect) {
    await waitFor(() => snapshot?.sessions.length === 1, 'Initial session did not load');
    assert(document.getElementById('microphone').disabled, 'Hardware controls are enabled');
    assert(inspect().devtools === true, 'Developer extras were not enabled by the host');
    report.steps.push('Hardware controls disabled, developer extras enabled');

    // A lost reply must become a failure, not a parked promise: an uncaught
    // error and a malformed request both have to surface without hanging.
    const known = inspect().errors.length;
    setTimeout(() => { window.__missingFn(); }, 0);
    await waitFor(() => inspect().errors.length === known + 1, 'Uncaught error was not captured in the diagnostics deque');
    assert(inspect().diagnostics.length <= 200, 'Diagnostics deque is unbounded');
    report.steps.push('Uncaught error captured in the diagnostics deque');

    const orphans = inspect().unknownReplies;
    window.webkit.messageHandlers.navi.postMessage(JSON.stringify({type: 'rpc', ticket: 424242, request: null}));
    await waitFor(() => inspect().unknownReplies === orphans + 1, 'Malformed request was not answered with its ticket');
    let rejected = null;
    try { await rpc('__no_such_op'); } catch (error) { rejected = error; }
    assert(rejected && /unknown operation/i.test(rejected.message), 'Unknown operation did not reject cleanly: ' + rejected);
    await waitFor(() => inspect().pending === 0, 'Pending RPCs never settled');
    report.steps.push('Malformed request answered with its ticket; unknown op rejected');

    // Slash list must surface the fake OpenCode catalog; stop must cancel an
    // in-flight decision (fixture inference is delayed 2 s).
    const command = document.getElementById('command');
    command.value = '/';
    command.dispatchEvent(new Event('input', {bubbles: true}));
    await waitFor(() => !document.getElementById('slash-list').hidden, 'Slash list did not open');
    await waitFor(() => document.getElementById('slash-list').children.length >= 3, 'Slash catalog did not populate');
    report.slash_count = document.getElementById('slash-list').children.length;
    command.value = '';
    command.dispatchEvent(new Event('input', {bubbles: true}));
    report.steps.push('Slash list rendered ' + report.slash_count + ' OpenCode commands');

    command.value = 'open something for me';
    document.getElementById('chat-form').requestSubmit();
    await waitFor(() => !document.getElementById('stop').hidden, 'Stop button did not appear during work');
    document.getElementById('stop').click();
    await waitFor(() => document.getElementById('state').textContent === 'Ready' && document.getElementById('stop').hidden, 'Stop did not cancel the request: ' + document.getElementById('state').textContent);
    report.steps.push('Stop cancelled an in-flight command');


    const before = snapshot.sessions.length;
    document.getElementById('command').value = 'Open Fixture shell';
    document.getElementById('chat-form').requestSubmit();
    await waitFor(() => [...document.querySelectorAll('#chat-log .msg.user p')].pop()?.textContent === 'Open Fixture shell', 'Typed message was not shown');
    report.steps.push('Typed message rendered in the chat panel');

    await waitFor(() => snapshot.sessions.length === before + 1, 'GUI command did not launch a registered harness');
    await waitFor(() => document.getElementById('state').textContent === 'Ready', 'Decision did not complete: ' + document.getElementById('state').textContent);
    report.steps.push('Chat command reached Rust and launched a registered harness');

    // Session strip must show the launched harness; clicking it opens the
    // attached terminal pane with the session's TUI.
    const sessionChips = () => [...document.querySelectorAll('#sessions .session-chip:not(.new):not(.recent-toggle)')];
    await waitFor(() => sessionChips().length === before + 1, 'Session chip did not render');
    sessionChips()[before].click();
    await waitFor(() => !document.getElementById('terminal-pane').hidden, 'Terminal pane did not open');
    await waitFor(() => document.querySelector('#terminal-host .xterm') !== null, 'xterm did not mount');
    report.steps.push('Terminal pane opened with xterm for the harness session');

    // Type into the terminal → tmux pane (the fixture shell echoes stdin).
    const term = window.navi?.inspect?.().terminal;
    assert(term === true, 'inspect().terminal should be true');
    document.getElementById('terminal-prompt').value = 'echo navi-probe-$((40+2))';
    document.getElementById('terminal-send').click();
    await waitFor(() => document.querySelector('#terminal-host .xterm-rows')?.textContent.includes('navi-probe-42'), 'Prompt output never appeared in the terminal');
    report.steps.push('Sent a prompt into the harness and observed its output');
    /* BUG-082: the terminal's Stop sends session_interrupt for this session. */
    const interruptCalls = inspect().rpcCalls.length;
    document.getElementById('terminal-stop').click();
    await waitFor(() => inspect().rpcCalls.slice(interruptCalls)
      .some(call => call.op === 'session_interrupt'),
      'The terminal Stop did not interrupt the harness');
    report.steps.push('Terminal Stop sent session_interrupt for the attached harness');


    // New-session dialog: launch a harness and inject its first prompt.
    openNewDialogFromChip();
    await waitFor(() => document.getElementById('new-dialog').open, 'New-session dialog did not open');
    document.getElementById('new-profile').value = 'fixture-shell';
    document.getElementById('new-prompt').value = 'echo navi-create-$((3+4))';
    document.getElementById('new-form').requestSubmit();
    await waitFor(() => !document.getElementById('new-dialog').open, 'New-session dialog did not close');
    await waitFor(() => !document.getElementById('terminal-pane').hidden && document.getElementById('terminal-title').textContent.includes('Fixture'), 'Terminal did not open for the new session');
    await waitFor(() => document.querySelector('#terminal-host .xterm-rows')?.textContent.includes('navi-create-7'), 'Injected prompt output never appeared');
    report.steps.push('New-session dialog launched a harness and injected its prompt');

    /* Per-row actions: copy reports through the toast, edit prefills the composer,
       and an assistant row exposes Retry only when the turn it belongs to has a
       request to resend (BUG-041). */
    const lastUserRow = [...document.querySelectorAll('#chat-log .msg.user')].pop();
    assert(lastUserRow, 'The chat has no user row to act on');
    lastUserRow.querySelector('.msg-action[data-action="copy"]').click();
    await waitFor(() => /copied|copy/i.test(document.getElementById('toast').textContent), 'Copy produced no feedback');
    lastUserRow.querySelector('.msg-action[data-action="edit"]').click();
    assert(document.getElementById('command').value === 'Open Fixture shell', 'Edit did not prefill the composer: ' + document.getElementById('command').value);
    document.getElementById('command').value = '';
    const lastReplyRow = [...document.querySelectorAll('#chat-log .msg.navi')].pop();
    assert(lastReplyRow && lastReplyRow.querySelector('.msg-action[data-action="retry"]'),
      'An assistant row has no Retry action');
    /* No finished session exists yet, so the Recent group is absent, not open. */
    const recentGroup = [...document.querySelectorAll('#sessions .sessions-recent')]
      .find(group => group.querySelector('.recent-toggle').textContent.startsWith('Recent'));
    assert(!recentGroup || recentGroup.hidden, 'Recent is open while every session is running');
    /* Sound starts on and its button saves the change. The settings form below turns
       it back on, so that save is a real change and not the default. */
    const sound = document.getElementById('speak-toggle');
    assert(snapshot.settings.speak === true, 'Spoken replies are not on by default');
    assert(sound.textContent === 'Sound on', 'The sound button reads ' + sound.textContent);
    sound.click();
    await waitFor(() => snapshot.settings.speak === false && sound.textContent === 'Sound off', 'The sound button did not turn sound off');
    report.steps.push('The sound button turned spoken replies off');
    /* The composer model switcher opens the same catalog and Escape closes it. */
    document.getElementById('settings-button').click();
    assert(document.getElementById('settings-dialog').open, 'Settings did not open');
    assert(document.getElementById('setting-model').value.length > 0, 'Model select not populated');
    assert(document.querySelectorAll('#profiles-editor .profile-row').length >= 4, 'Harness profiles not listed');
    /* Connections: an isolated station has no account and no browser to open. */
    await waitFor(() => document.getElementById('mail-state').textContent === 'Email: not connected',
      'Mail state reads ' + document.getElementById('mail-state').textContent);
    assert(document.getElementById('browser-state').textContent.startsWith('Browser:'), 'The browser state is empty');
    const shown = id => getComputedStyle(document.getElementById(id)).display !== 'none';
    assert(shown('mail-form'), 'The email form is hidden while not connected');
    assert(!shown('mail-connected'), 'Disconnect is offered while not connected');
    const tick = document.getElementById('setting-speak').getBoundingClientRect();
    assert(tick.width === 13 && tick.height === 13, `A checkbox is ${tick.width}x${tick.height}`);
    assert(document.getElementById('mail-connect').disabled, 'Connect is enabled in isolated mode');
    assert([...document.querySelectorAll('#account-links button')].every(button => button.disabled),
      'An account button is enabled in isolated mode');
    report.steps.push('Connections showed email and browser state, and offered nothing an isolated station cannot do');
    await waitFor(() => document.getElementById('diagnostics-body')?.textContent.includes('renderer'), 'Diagnostics block did not render');
    assert(document.getElementById('diagnostics-reload'), 'Diagnostics reload button missing');
    assert(document.getElementById('diagnostics-copy'), 'Diagnostics copy button missing');
    report.diagnostics = document.getElementById('diagnostics-body').textContent;
    report.steps.push('Settings rendered live diagnostics with no network');
    /* BUG-044: the fixture catalog does not contain the saved model, so opening the
       picker and saving must keep it — the old select blanked it and broke every turn. */
    const savedModel = snapshot.settings.assistant.model;
    assert(savedModel, 'The daemon reported no assistant model');
    /* BUG-077: a profile arg with a space survives the editor and the daemon's
       settings round-trip as one array element, not two split words. */
    document.getElementById('profile-add').click();
    const argsRow = [...document.querySelectorAll('#profiles-editor .profile-row')].pop();
    argsRow.querySelector('.profile-name').value = 'args-probe';
    argsRow.querySelector('.profile-exec').value = '/bin/true';
    argsRow.querySelector('.profile-args').value = '["--flag", "with space"]';
    document.getElementById('setting-language').value = 'fr';
    document.getElementById('setting-speak').checked = true;
    document.getElementById('setting-prompt').value += ' Keep it short.';
    document.getElementById('settings-form').requestSubmit();
    await waitFor(() => snapshot.settings.speak === true,
      'Speak setting did not save (toast: ' + document.getElementById('toast').textContent
      + ' field: ' + document.getElementById('setting-model').value
      + ' pending: ' + JSON.stringify(inspect().pendingOps)
      + ' errors: ' + JSON.stringify(inspect().diagnostics.slice(-2)) + ')');
    await waitFor(() => snapshot.settings.system_prompt.endsWith('Keep it short.'), 'System prompt did not save');
    await waitFor(() => !document.getElementById('settings-dialog').open, 'Settings stayed open after save');
    assert(snapshot.settings.assistant.model === savedModel,
      'Saving settings replaced the model: ' + snapshot.settings.assistant.model);
    const persisted = await rpc('status');
    const argsProbe = (persisted.settings.profiles || []).find(profile => profile.name === 'args-probe');
    assert(argsProbe && argsProbe.args.length === 2 && argsProbe.args[1] === 'with space',
      'Args with spaces did not round-trip: ' + JSON.stringify(argsProbe && argsProbe.args));
    // BUG-057: the speech language is persisted, not hardcoded to English.
    assert(persisted.settings.speech && persisted.settings.speech.language === 'fr',
      'Speech language did not save: ' + JSON.stringify(persisted.settings.speech));
    report.steps.push('Settings saved: speak on, Navi prompt updated, model kept, spaced args and language kept');

    /* The picker itself: focusing the field lists the catalog grouped by provider,
       typing narrows it, and choosing persists the reference. */
    document.getElementById('settings-button').click();
    await waitFor(() => document.getElementById('settings-dialog').open, 'Settings did not reopen');
    const modelField = document.getElementById('setting-model');
    modelField.focus();
    await waitFor(() => !document.getElementById('model-options').hidden, 'The model list did not open');
    modelField.value = 'fixture';
    modelField.dispatchEvent(new Event('input', {bubbles: true}));
    /* The catalog read is asynchronous: a list that lands later must reach the field. */
    await waitFor(() => inspect().modelPicker.catalog >= 1, 'The model catalog never arrived: ' + JSON.stringify(inspect().modelPicker));
    modelField.dispatchEvent(new Event('input', {bubbles: true}));
    await waitFor(() => document.querySelectorAll('#model-options .combobox-option').length >= 1,
      'Searching the model catalog returned nothing: ' + JSON.stringify(inspect().modelPicker)
      + ' note=' + document.getElementById('model-note').textContent
      + ' log=' + JSON.stringify(inspect().diagnostics.slice(-3)));
    const fixtureModel = document.querySelector('#model-options .combobox-option[data-value="fixture/f-1"]');
    assert(fixtureModel, 'The fixture model is missing from the filtered list: '
      + [...document.querySelectorAll('#model-options .combobox-option')].map(node => node.dataset.value).join(','));
    assert(document.querySelectorAll('#model-options .combobox-group').length >= 1, 'The model list has no provider group');
    fixtureModel.dispatchEvent(new MouseEvent('mousedown', {bubbles: true}));
    assert(modelField.value === 'fixture/f-1', 'Choosing a model did not fill the field: ' + modelField.value);
    document.getElementById('settings-form').requestSubmit();
    await waitFor(() => snapshot.settings.assistant.model === 'fixture/f-1',
      'The chosen model did not persist: ' + snapshot.settings.assistant.model);
    await waitFor(() => !document.getElementById('settings-dialog').open, 'Settings stayed open after the model save');
    report.steps.push('Model picker searched the catalog, grouped providers and persisted fixture/f-1');
    /* The inline switcher is the same catalog, anchored inside the chat panel, and
       Escape closes it without changing anything. */
    document.getElementById('model-switcher').click();
    assert(!document.getElementById('model-pop').hidden, 'The composer model menu did not open');
    assert(document.querySelectorAll('#model-list .combobox-option').length >= 1, 'The composer model menu is empty');
    const popup = document.getElementById('model-pop').getBoundingClientRect();
    assert(popup.top >= 0 && popup.bottom <= innerHeight, 'The composer model menu leaves the viewport: ' + JSON.stringify(popup));
    document.getElementById('model-search').dispatchEvent(new KeyboardEvent('keydown', {key: 'Escape', bubbles: true}));
    assert(document.getElementById('model-pop').hidden, 'Escape did not close the composer model menu');
    report.steps.push('Composer model switcher opened inside the panel and closed with Escape');

    // Kinds the daemon may add later must render status without throwing: the feed
    // is replayed through the same applier that consumes polled status. Synthetic
    // sequences are negative, so they de-duplicate like real ones but can never
    // move the poll cursor past the daemon's own events.
    let feedSeq = 0;
    /* `busy` is authoritative in the client, so a fabricated turn has to claim it. */
    const feed = kinds => ({...snapshot, busy: kinds.some(kind => kind !== 'reply'),
      events: kinds.map(kind => ({kind, seq: -(++feedSeq),
        time: Date.now(), message: `probe ${kind}`, details: {name: 'fixture'}}))});
    const feedErrors = inspect().errors.length;
    const chip2 = () => document.getElementById('state').textContent;
    /* The model changes synchronously; the rows paint on the next frame, so DOM
       assertions wait one frame instead of racing the painter. */
    const painted2 = () => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)));
    update(feed(['reasoning']));
    assert(/thinking/i.test(chip2()), 'Reasoning did not label the state: ' + chip2());
    update(feed(['tool_use']));
    assert(/working/i.test(chip2()), 'Tool use did not surface its work state: ' + chip2());
    update(feed(['approval']));
    assert(/approval needed/i.test(chip2()), 'Approval did not surface: ' + chip2());
    update(feed(['warning', 'unlisted_kind']));
    assert(inspect().errors.length === feedErrors, 'A newer event kind was recorded as an error');
    await painted2();
    assert([...document.querySelectorAll('#chat-log .part.tone-warning')]
      .some(node => node.textContent.includes('probe warning')), 'A warning event did not render a warning row');
    update(feed(['reply']));
    await waitFor(() => chip2() === 'Ready', 'The probe turn did not close: ' + chip2());
    await painted2();
    assert(document.querySelector('#chat-log .part.tool') !== null, 'The tool chip never rendered');
    report.steps.push('Reasoning, tool_use, approval, warning and unknown kinds rendered without errors');

    report.messages = document.querySelectorAll('#chat-log .msg').length;
    report.sessions = snapshot.sessions;
    report.model = snapshot.settings.model.name;
    report.voice_engine = snapshot.settings.speech.engine;
    report.renderer = document.getElementById('field').dataset.renderer;
    report.dpr = window.navi.inspect().dpr;
    report.pixel_ratio = window.navi.inspect().pixelRatio;

    // A stage failure must leave the shell usable, not a blank window. The stage
    // owns the failure; the station owns the recovery contract.
    window.dispatchEvent(new CustomEvent('navi:degraded', {detail: {reason: 'workflow probe'}}));
    await waitFor(() => window.navi.inspect().degraded === true, 'Degradation was not recorded');
    assert(document.getElementById('field').dataset.renderer === 'fallback', 'Renderer was not reported honestly');
    assert(window.navi.inspect().booted === true, 'Boot flag was not set after degradation');
    assert(window.navi.inspect().chatOpacity === '1', 'Chat stayed hidden after degradation');
    report.steps.push('Degraded stage kept a visible chat with an honest renderer');
    report.ok = true;
  }

  /* `--chat`: the whole chat contract is exercised through the global `update()`,
   * so no model, gateway or OpenCode fixture is involved. Synthetic events carry
   * negative sequence numbers: they de-duplicate exactly like real ones and can
   * never move the poll cursor past the daemon's own events. */
  async function runChat(inspect) {
    const chip = () => document.getElementById('state').textContent;
    const log = () => document.getElementById('chat-log');
    const input = () => document.getElementById('command');
    const rows = kind => [...document.querySelectorAll('#chat-log .msg' + (kind ? '.' + kind : ''))];
    const painted = () => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)));
    const errorRows = () => rows().filter(row => row.querySelector('.part.error')).length;
    let fake = 0;
    const feed = async (kind, details = {}, message = '') => {
      fake += 1;
      const base = {...snapshot};
      delete base.turn_state;   // a synthetic event must not replay the host's coalesced turn
      /* `busy` is authoritative: a fabricated turn claims it, and only the terminal
         kinds release it, exactly as the daemon would. */
      base.busy = kind !== 'reply' && kind !== 'error' && kind !== 'stopped';
      update({...base, events: [{seq: -fake, time: Date.now(), kind, message, details}]});
      await painted();
    };

    assert(chip() !== 'starting', 'The status chip is still the boot placeholder: ' + chip());
    await waitFor(() => chip() === 'Ready', 'A healthy poll did not report Ready: ' + chip());
    assert(!document.getElementById('chat-empty').hidden, 'The first-run block never rendered');
    report.chat = {status: chip(), empty: true};
    /* From here the page fabricates turns: hold the status feed so the daemon's own
       idle payload cannot (correctly) end them, and re-poll at the end. */
    window.navi.hooks.pausePoll(true);

    /* Paging (BUG-047): three saved messages in a project of their own, so the
       chat run's own transcript is still empty at boot. The first page holds the
       newest two; scrolling to the top asks for the page in front of the oldest
       cursor, gets exactly the one older row, and then stops asking. */
    const chatLog = log();
    const realProjects = snapshot.projects;
    const historyCalls = () => inspect().rpcCalls.filter(call => call.op === 'messages').length;
    /* The active project is client state now (BUG-090): the probe rows are scoped
       by giving the page one project of its own, not by writing a setting. */
    update({...snapshot, projects: [{id: 'paging-probe', name: 'Paging probe',
      path: '/tmp/paging-probe', path_state: 'ok'}]});
    window.navi.hooks.resetChat();
    await window.navi.hooks.hydrate({limit: 2});
    assert(inspect().chat.ids.join(',') === 'paging-1,paging-2',
      'The first page is not the newest pair: ' + inspect().chat.ids.join(','));
    assert(inspect().chat.history.hasMore === true, 'The first page did not report older history');
    const oldest = inspect().chat.history.oldestCursor;
    assert(typeof oldest === 'number', 'The first page carried no cursor: ' + oldest);
    const calls = historyCalls();
    chatLog.scrollTop = 0;
    chatLog.dispatchEvent(new Event('scroll'));
    await waitFor(() => inspect().chat.count === 3, `Paging loaded ${inspect().chat.count} of 3 messages`);
    assert(inspect().chat.ids.join(',') === 'paging-0,paging-1,paging-2',
      'Paging left a gap or a duplicate: ' + inspect().chat.ids.join(','));
    assert(inspect().chat.history.hasMore === false, 'The store still claimed older history');
    assert(historyCalls() === calls + 1, 'Paging sent ' + (historyCalls() - calls) + ' history requests');
    assert(document.querySelectorAll('#chat-log .msg').length === 3,
      'The log shows ' + document.querySelectorAll('#chat-log .msg').length + ' rows for 3 messages');
    chatLog.dispatchEvent(new Event('scroll'));
    await painted();
    assert(inspect().chat.count === 3 && historyCalls() === calls + 1,
      'Paging asked the store again after has_more:false');
    report.chat.paging = {page: 2, loaded: 3, hasMore: false, before: oldest,
      cursors: inspect().chat.cursors.slice()};
    /* Back to the chat run's own project; the probe rows are not part of it. */
    window.navi.hooks.resetChat();
    update({...snapshot, projects: realProjects});
    await window.navi.hooks.hydrate();
    assert(inspect().chat.count === 0, 'The probe rows survived the project switch');

    /* Status is a projection of the payload: a turn reads Working, its reply Ready. */
    await feed('working', {}, 'working');
    assert(chip() === 'Working', 'A working turn did not read Working: ' + chip()
      + ' busy=' + inspect().busy + ' chat=' + JSON.stringify(inspect().chat)
      + ' log=' + JSON.stringify(inspect().diagnostics.slice(-2)));
    await feed('reply', {text: 'Probe answer one'}, 'Probe answer one');
    await waitFor(() => chip() === 'Ready', 'The finished turn did not return to Ready: ' + chip());
    assert(document.getElementById('chat-empty').hidden, 'The first-run block stayed after a message');
    report.chat.ready = chip();

    /* Streaming: the row exists on the first delta, grows between two samples, and
       the final reply replaces it in place — exactly one row for the turn. */
    const naviBefore = rows('navi').length;
    await feed('reply_delta', {text: 'Streaming '});
    await waitFor(() => rows('navi').length === naviBefore + 1, 'The streaming row was never created');
    const live = rows('navi').at(-1);
    const liveId = live.dataset.id;
    assert(live.dataset.status === 'streaming', 'The streaming row is not marked streaming: ' + live.dataset.status);
    const firstSample = live.textContent.length;
    await new Promise(resolve => setTimeout(resolve, 180));
    await feed('reply_delta', {text: 'text that arrives in pieces'});
    const secondSample = rows('navi').at(-1).textContent.length;
    assert(secondSample > firstSample, `The streaming row did not grow: ${firstSample} → ${secondSample}`);
    await feed('reply', {text: 'Streaming text that arrives in pieces'}, 'Streaming text');
    await waitFor(() => rows('navi').at(-1).dataset.status === 'complete', 'The turn never finalised');
    assert(rows('navi').filter(row => row.dataset.id === liveId).length === 1, 'The turn rendered more than one row');
    assert(rows('navi').at(-1).textContent.includes('arrives in pieces'), 'The final text did not replace the stream');
    report.chat.streaming = {first: firstSample, second: secondSample};

    /* The host's push channel (`navi.push`) may deliver a coalesced `turn_state`
       instead of chunks: it replaces the live row's text and `done` ends the turn. */
    await feed('reply_delta', {text: 'Chunk that a coalesced payload will replace '});
    update({...snapshot, busy: true, turn_state: {turn: 'coalesced-1', text: 'Coalesced answer', done: false}});
    await painted();
    const coalesced = rows('navi').at(-1);
    assert(coalesced.textContent.includes('Coalesced answer'), 'turn_state did not replace the live text: ' + coalesced.textContent);
    assert(!coalesced.textContent.includes('will replace'), 'turn_state appended instead of replacing');
    assert(inspect().chat.status === 'working', 'turn_state(done:false) did not keep the turn working: ' + inspect().chat.status);
    update({...snapshot, busy: false, turn_state: {turn: 'coalesced-1', text: 'Coalesced answer', done: true}});
    await painted();
    assert(chip() === 'Ready', 'turn_state(done:true) did not finish the turn: ' + chip());
    assert(coalesced.dataset.status === 'complete', 'turn_state(done:true) left the row streaming');
    await feed('reply', {turn: 'coalesced-1', text: 'Coalesced answer'}, 'Coalesced answer');
    assert(rows('navi').filter(row => row.textContent.includes('Coalesced answer')).length === 1,
      'The coalesced turn rendered a second row');
    report.chat.turn_state = {replaced: true, done: true};

    /* De-duplication is by identity, never by text: identical answers with different
       ids render twice, one event delivered twice renders once, and one stable
       message id collapses two deliveries into one row. */
    const before = rows().length;
    await feed('reply', {text: 'Same words twice'}, 'Same words twice');
    await feed('reply', {text: 'Same words twice'}, 'Same words twice');
    await waitFor(() => rows().length === before + 2, `Identical text with different ids must render twice, got ${rows().length - before}`);
    update({...snapshot, events: [{seq: -fake, time: Date.now(), kind: 'reply', message: 'Same words twice', details: {text: 'Same words twice'}}]});
    await painted();
    assert(rows().length === before + 2, 'Re-delivering one event rendered it twice');
    await feed('reply', {id: 'fixed-message', text: 'One identity'}, 'One identity');
    await feed('reply', {id: 'fixed-message', text: 'One identity'}, 'One identity');
    await painted();
    assert(rows().length === before + 3, 'One message id rendered more than one row: ' + (rows().length - before));
    report.chat.dedupe = {rows: rows().length};

    /* A tool call is one collapsible chip per id, upserted in place: three events,
       one chip, one final state. */
    await feed('tool_use', {id: 'tool-1', name: 'shell', status: 'running', input: 'git status'}, 'shell · running');
    await feed('tool_use', {id: 'tool-1', name: 'shell', status: 'completed', output: '## main', exit: 0, final: true}, 'shell · completed');
    await feed('tool_use', {id: 'tool-1', name: 'shell', status: 'completed', output: '## main', exit: 0, final: true}, 'shell · completed');
    await waitFor(() => document.querySelectorAll('#chat-log .part.tool[data-tool="tool-1"]').length === 1, 'The tool chip was duplicated');
    const tool = document.querySelector('#chat-log .part.tool[data-tool="tool-1"]');
    assert(tool.dataset.final === 'true' && tool.dataset.status === 'completed', 'The tool chip never reached its final state: ' + tool.dataset.status);
    assert(tool.querySelectorAll('summary').length === 1, 'The tool chip has more than one header');
    report.chat.tool = {status: tool.dataset.status};

    /* Reasoning is collapsed, an approval carries Allow/Deny, and one failure is
       one surface: the rejection and the matching error event never both render. */
    await feed('reasoning', {text: 'Checking the fixture before answering'}, 'Assistant reasoning');
    await waitFor(() => document.querySelector('#chat-log .part.reasoning') !== null, 'Reasoning did not render');
    const reasoning = document.querySelector('#chat-log .part.reasoning');
    assert(reasoning.tagName === 'DETAILS' && reasoning.open === false, 'Reasoning is not collapsed');
    await feed('approval', {id: 'permission-1', tool: 'shell', input: 'rm -rf build'}, 'shell needs approval');
    const approval = document.querySelector('#chat-log .part.tool[data-status="approval"]');
    assert(approval !== null, 'The approval row did not render');
    assert(approval.querySelector('.approval-allow') && approval.querySelector('.approval-deny'), 'Allow/Deny are missing');
    await feed('reply', {text: 'Ready to continue'}, 'Ready to continue');
    await waitFor(() => chip() === 'Ready', 'The approval turn did not close: ' + chip());
    const ops = inspect().rpcOps.length;
    approval.querySelector('.approval-deny').click();
    await waitFor(() => inspect().rpcOps.slice(ops).includes('permission_reply'), 'Deny did not send permission_reply');
    await waitFor(() => errorRows() === 1, 'The failed permission reply did not render exactly one error row: ' + errorRows());
    const failure = inspect().chat.lastFailure;
    assert(failure, 'The failed permission reply recorded no failure');
    const refusalRow = rows().find(row => row.querySelector('.part.error'));
    assert(refusalRow && refusalRow.textContent.includes(failure),
      'The refusal row does not show the daemon text: ' + (refusalRow ? refusalRow.textContent : 'no row'));
    /* BUG-006: a refused decision is a failure surface, never a decision. */
    assert(approval.querySelector('.approval-allow') !== null, 'A refused decision was rendered as decided');
    await feed('error', {error: failure}, failure);
    assert(errorRows() === 1, 'One failure produced ' + errorRows() + ' error surfaces');

    /* The same refusal with a text the daemon owns end to end: an id it cannot
       post is rejected with one sentence, and the UI shows that sentence once. */
    await feed('approval', {id: 'permission-\u0001bad', tool: 'shell', input: 'rm -rf build'}, 'shell needs approval');
    const second = [...document.querySelectorAll('#chat-log .part.tool[data-status="approval"]')].at(-1);
    assert(second !== null, 'The second approval row did not render');
    const secondOps = inspect().rpcOps.length;
    second.querySelector('.approval-allow').click();
    await waitFor(() => inspect().rpcOps.slice(secondOps).includes('permission_reply'), 'Allow did not send permission_reply');
    await waitFor(() => errorRows() === 2,
      'The daemon refusal did not render exactly one more row: ' + errorRows());
    await painted();
    const refusedText = 'Invalid permission id';
    const refusedRow = rows().filter(row => row.querySelector('.part.error')).at(-1);
    assert(refusedRow.textContent.includes(refusedText),
      'The daemon error text is missing from the row: ' + refusedRow.textContent);
    assert(rows().filter(row => row.textContent.includes(refusedText)).length === 1,
      'The refusal rendered on more than one row');
    assert(second.querySelector('.approval-allow') !== null, 'The refusal was rendered as a decision');
    assert(!/Decision: /.test(second.textContent), 'The refusal changed the approval label: ' + second.textContent);
    report.chat.errors = {rows: errorRows()};
    report.chat.permission = {rows: errorRows(), text: refusedText, decided: false,
      replies: inspect().rpcCalls.filter(call => call.op === 'permission_reply')
        .map(call => call.params.decision)};

    /* Markdown: a fenced block becomes code with a copy affordance, and raw HTML is
       text — no element is ever created from model output. */
    const payload = 'Here is the fix:\n\n```js\nconst x = 1 < 2;\n```\n\n<img src=x onerror="window.__xss=1">';
    await feed('reply', {text: payload}, 'Fix');
    await waitFor(() => document.querySelector('#chat-log pre code') !== null, 'A fenced block did not render');
    const code = [...document.querySelectorAll('#chat-log pre code')].at(-1);
    assert(code.textContent.includes('const x = 1 < 2;'), 'The code text was altered: ' + code.textContent);
    assert(document.querySelector('#chat-log .code-copy') !== null, 'The code block has no copy affordance');
    assert(document.querySelector('#chat-log img') === null, 'Raw HTML became an element');
    assert(!window.__xss, 'A raw onerror attribute executed');
    assert(log().textContent.includes('<img src=x onerror='), 'The tag was not rendered as text');
    report.chat.markdown = {code: true, safe: true};

    /* Persistence: the saved transcript comes back from the daemon's store, in the
       same order, after the local model is thrown away. */
    const saved = inspect().chat.savedIds.slice();
    window.navi.hooks.resetChat();
    assert(inspect().chat.count === 0 && !document.getElementById('chat-empty').hidden, 'Reset did not clear the log');
    for (let attempt = 0; attempt < 10; attempt += 1) {
      await window.navi.hooks.hydrate();
      if (inspect().chat.savedIds.length === saved.length) break;
      await new Promise(resolve => setTimeout(resolve, 200));
    }
    assert(inspect().chat.savedIds.length === saved.length,
      `Hydration returned ${inspect().chat.savedIds.length} of ${saved.length} saved messages`);
    assert(inspect().chat.savedIds.join(',') === saved.join(','), 'Hydrated order differs from the saved order');
    report.chat.rehydrated = saved.length;

    /* Voice, BUG-056: the transcript lands in the composer with a countdown, and
       Enter claims the hold from the daemon before it dispatches — the worker that
       dispatched at the deadline already owns the command, so a lost claim must
       send nothing and say so. */
    const claimed = () => inspect().rpcCalls
      .filter(call => call.op === 'voice_confirm' && call.params.accept === true).length;
    const cancelled = () => inspect().rpcCalls
      .filter(call => call.op === 'voice_confirm' && call.params.accept === false).length;
    const commands = () => inspect().rpcCalls.filter(call => call.op === 'command').length;
    update({...snapshot, busy: false, voice: {status: 'confirming', transcript: 'list the failing tests',
      confirm_seconds: 1.5, source: 'deepgram', confidence: 0.82}});
    assert(input().value === 'list the failing tests', 'The transcript did not reach the composer: ' + input().value);
    const banner = document.getElementById('voice-banner');
    assert(!banner.hidden, 'The voice banner did not open');
    assert(/[0-9]\.[0-9]s/.test(document.getElementById('voice-count').textContent),
      'The countdown did not render: ' + document.getElementById('voice-count').textContent);
    const source = document.getElementById('voice-source').textContent;
    assert(source.includes('deepgram') && source.includes('82%'), 'Source/confidence missing: ' + source);
    const claims = claimed(), sent = commands();
    input().dispatchEvent(new KeyboardEvent('keydown', {key: 'Enter', bubbles: true, cancelable: true}));
    assert(input().value === '', 'The composer kept the dispatched transcript');
    assert(banner.hidden, 'The banner stayed open after the dispatch');
    await waitFor(() => claimed() === claims + 1, 'Enter did not claim the transcript from the daemon');
    await waitFor(() => commands() === sent + 1, 'The claimed transcript was not dispatched exactly once');
    await waitFor(() => rows('user').some(row => row.textContent.includes('list the failing tests')),
      'The dispatched transcript never rendered');
    assert(rows('user').filter(row => row.textContent.includes('list the failing tests')).length === 1,
      'The claimed transcript rendered more than one user row');
    await waitFor(() => chip() !== 'Working', 'The claimed turn never settled: ' + chip());

    /* The worker won the race: the claim already exists, so the daemon answers
       `claimed:false` and the UI must not repeat the command. */
    update({...snapshot, busy: false, voice: {status: 'confirming', transcript: 'run the smoke tests',
      confirm_seconds: 1.5, source: 'deepgram'}});
    assert(input().value === 'run the smoke tests', 'The second transcript did not reach the composer: ' + input().value);
    const secondCount = inspect().chat.count, secondCommands = commands();
    input().dispatchEvent(new KeyboardEvent('keydown', {key: 'Enter', bubbles: true, cancelable: true}));
    await waitFor(() => claimed() === claims + 2, 'The second Enter did not ask the daemon to claim');
    await waitFor(() => /already sent/i.test(document.getElementById('toast').textContent),
      'The losing claim showed no note: ' + document.getElementById('toast').textContent);
    await painted();
    assert(document.getElementById('toast').classList.contains('visible'), 'The note is not visible');
    assert(commands() === secondCommands, 'The losing claim dispatched a second command');
    assert(inspect().chat.count === secondCount, 'The losing claim appended a message');

    /* Escape withdraws the hold instead of sending it, and keeps the draft. */
    update({...snapshot, busy: false, voice: {status: 'confirming', transcript: 'check the logs',
      confirm_seconds: 1.5, source: 'deepgram'}});
    assert(input().value === 'check the logs', 'The third transcript did not reach the composer: ' + input().value);
    const cancels = cancelled(), escaped = commands();
    input().dispatchEvent(new KeyboardEvent('keydown', {key: 'Escape', bubbles: true, cancelable: true}));
    await waitFor(() => cancelled() === cancels + 1, 'Escape did not withdraw the hold');
    await painted();
    assert(banner.hidden, 'The banner stayed open after Escape');
    assert(input().value === 'check the logs', 'Escape dropped the draft instead of offering it');
    assert(commands() === escaped, 'Escape dispatched the transcript');
    report.chat.voice = {confirming: true, claimed: true, dispatched: true, already: true, cancel: true,
      claims: claimed(), cancels: cancelled(), commands: commands()};

    /* Attachments (BUG-005): a small file is read once and sent with the prompt;
       an oversized file is refused before it can reach the daemon. */
    const attachBase = inspect().chat.attachments.length;
    const attached = window.navi.hooks.attach([
      {name: 'notes.txt', data: btoa('hello from a file')}]);
    assert(attached.accepted === 1, 'The small attachment was not accepted: ' + JSON.stringify(attached));
    assert(inspect().chat.attachments.length === attachBase + 1,
      'The attachment chip was not recorded');
    const attachmentChips = document.querySelectorAll('#attachment-chips .attachment-chip');
    assert(attachmentChips.length === 1, 'The attachment chip did not render');
    const beforeCommands = commands();
    input().value = 'summarise the attachment';
    document.getElementById('chat-form').requestSubmit();
    await waitFor(() => commands() === beforeCommands + 1,
      'The attachment prompt never reached the daemon');
    const attachedCommand = inspect().chat.lastCommand;
    assert(attachedCommand && attachedCommand.attachments.join(',') === 'notes.txt',
      'The prompt did not carry the attachment: ' + JSON.stringify(attachedCommand));
    const refused = window.navi.hooks.attach([
      {name: 'huge.bin', data: 'A'.repeat(3 * 1024 * 1024)}]);
    assert(refused.accepted === 0, 'A 3 MiB attachment was accepted: ' + JSON.stringify(refused));
    assert(inspect().chat.attachments.length === 0, 'The refused attachment left a chip behind');
    assert(/larger than 2 MiB/.test(inspect().chat.lastFailure || ''),
      'The refusal was not surfaced: ' + inspect().chat.lastFailure);
    report.chat.attachments = {sent: 1, refused: true, names: attachedCommand.attachments};

    report.chat.count = inspect().chat.count;
    report.chat.empty_final = inspect().chat.empty;
    report.chat.label = chip();
    window.navi.hooks.pausePoll(false);   // the live feed takes over again
    report.steps.push('Status derived Ready → Working → Ready without a "starting" placeholder');
    report.steps.push(`Streaming row grew ${firstSample}→${secondSample} chars and finalised in place`);
    report.steps.push('Identical text with different ids rendered twice; one id rendered once');
    report.steps.push('One tool chip per id, upserted to its single final state');
    report.steps.push('Reasoning collapsed, approval sent permission_reply, one error surface per failure');
    report.steps.push('Fenced code rendered with a copy affordance; raw HTML stayed text');
    report.steps.push(`Transcript rehydrated from the daemon store (${report.chat.rehydrated} messages)`);
    report.steps.push('Older history paged by cursor: 3 saved rows in two pages, one request, has_more false');
    report.steps.push('Voice confirm claimed the hold before dispatch; a lost claim sent nothing and said so; Escape cancelled');
    report.steps.push('A refused permission decision stayed undecided and showed the daemon text once');
    report.steps.push('A text attachment was read and sent with the prompt; a 3 MiB file was refused visibly');
    report.ok = true;
  }

  try {
    const inspect = () => window.navi.inspect();
    await waitFor(() => (snapshot?.sessions || []).length >= 1, 'Initial session did not load');
    /* The isolated host loads this one script for every QA mode and injects no
     * page flag of its own, so the mode travels on the session the test created:
     * `--layout` names its session "Layout probe". */
    const layout = (snapshot?.sessions || []).some(session => (session.name || '').startsWith('Layout probe'));
    const chat = (snapshot?.sessions || []).some(session => (session.name || '').startsWith('Chat probe'));
    if (layout) await runLayout(inspect);
    else if (chat) await runChat(inspect);
    else await runWorkflow(inspect);
  } catch (error) {
    report.error = String(error);
  }
  window.naviQA = report;
})();
