/* Injected by the private GUI test host; never loaded by the application. */
(async () => {
  const report = {ok: false, steps: []};
  const assert = (condition, message) => {if (!condition) throw new Error(message);};
  async function waitFor(condition, message) {
    const deadline = performance.now() + 6000;
    while (!condition()) {
      if (performance.now() > deadline) throw new Error(message);
      await new Promise(resolve => setTimeout(resolve, 50));
    }
  }
  try {
    await waitFor(() => snapshot?.sessions.length === 1, 'Initial session did not load');
    const initial = snapshot.sessions[0].id;
    assert(document.getElementById('microphone').disabled && document.getElementById('desktop').disabled, 'Hardware controls are enabled');
    report.steps.push('Hardware controls disabled');

    document.getElementById('settings-button').click();
    assert(document.getElementById('settings-dialog').open, 'Settings did not open');
    document.getElementById('model-endpoint').value = 'https://example.com/v1/systemone';
    document.getElementById('settings-form').requestSubmit();
    await waitFor(() => document.getElementById('toast').textContent.includes('127.0.0.1'), 'Invalid endpoint was not rejected');
    assert(document.getElementById('settings-dialog').open, 'Invalid settings closed the dialog');
    report.steps.push('Remote decision endpoint rejected');

    document.getElementById('model-endpoint').value = snapshot.settings.model.endpoint;
    document.getElementById('model-name').value = 'GUI replacement provider';
    document.getElementById('model-threads').value = '1';
    document.getElementById('model-gpu').value = '0';
    document.getElementById('speech').value = JSON.stringify({engine: 'whisper'});
    document.getElementById('listening').value = 'gui';
    document.getElementById('settings-form').requestSubmit();
    await waitFor(() => !document.getElementById('settings-dialog').open && snapshot.settings.model.name === 'GUI replacement provider', 'Provider settings did not save');
    assert(snapshot.settings.model.threads === 1 && snapshot.settings.speech.engine === 'whisper' && snapshot.settings.listening === 'gui', 'Replacement settings differ');
    report.steps.push('Decision and speech configuration saved');

    document.getElementById('browser-folder').click();
    await waitFor(() => document.getElementById('toast').textContent.includes('disabled in isolated mode'), 'Desktop launcher was not blocked');
    report.steps.push('External desktop launcher blocked');

    document.getElementById('new-session').click();
    document.getElementById('launch-profile').value = 'fixture-shell';
    document.getElementById('launch-name').value = 'GUI review';
    document.getElementById('launch-form').requestSubmit();
    await waitFor(() => snapshot.sessions.length === 2 && document.getElementById('session-name').textContent === 'GUI review', 'Launch did not create and attach a terminal');
    report.steps.push('Named fixture harness launched and attached');

    const sessionButton = [...document.querySelectorAll('#sessions button')].find(button => button.querySelector('strong').textContent === 'Isolated validation');
    assert(sessionButton, 'Original session button missing');
    sessionButton.click();
    await waitFor(() => active === initial && document.getElementById('session-name').textContent === 'Isolated validation', 'Session did not switch');
    report.steps.push('Session selection switched the terminal');

    document.getElementById('command').value = 'Open Fixture shell';
    document.getElementById('command-form').requestSubmit();
    await waitFor(() => snapshot.sessions.length === 3, 'GUI command did not reach the typed provider');
    await waitFor(() => document.getElementById('state').textContent === 'ready', 'First decision did not complete');
    report.steps.push('GUI command reached Rust and launched a registered harness');

    const generation = snapshot.generation;
    document.getElementById('command').value = 'Open Fixture shell';
    document.getElementById('command-form').requestSubmit();
    await waitFor(() => document.getElementById('state').textContent === 'deciding', 'Second decision did not start');
    document.getElementById('stop').click();
    await waitFor(() => snapshot.generation > generation, 'Stop did not cancel the generation');
    await new Promise(resolve => setTimeout(resolve, 2500));
    assert(snapshot.sessions.length === 3, 'Canceled command launched a terminal');
    report.steps.push('Stop canceled pending inference without a terminal side effect');
    report.sessions = snapshot.sessions;
    report.model = snapshot.settings.model.name;
    report.voice_engine = snapshot.settings.speech.engine;
    report.renderer = document.getElementById('field').dataset.renderer;
    report.ok = true;
  } catch (error) {
    report.error = String(error);
  }
  window.naviQA = report;
})();
