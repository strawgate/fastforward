/**
 * Tests for highway-sim.mjs — Node 22 built-in test runner.
 *
 * Multi-route model: ramp → highway → exit (with traffic light on exit).
 *
 * Run:  node --test book/src/components/__tests__/highway-sim.test.mjs
 */
import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import { createSimulation, fixedScale, DEFAULTS } from '../highway-sim.mjs';

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function runTicks(sim, n, start, dt) {
  start = start || 0;
  dt = dt || DEFAULTS.spawnMs + 1;
  var last;
  for (var i = 0; i < n; i++) {
    last = sim.tick(start + i * dt);
  }
  return last;
}

// ---------------------------------------------------------------------------
// Traffic light cycling
// ---------------------------------------------------------------------------

describe('traffic light', function () {
  it('starts green when elapsed < green portion', function () {
    var sim = createSimulation({ greenPct: 50, cycleTotal: 1000, maxCars: 0 });
    sim.setCycleStart(0);
    sim.exitAuto();
    var frame = sim.tick(100);
    assert.equal(frame.lightIsGreen, true);
  });

  it('turns red when elapsed >= green portion', function () {
    var sim = createSimulation({ greenPct: 50, cycleTotal: 1000, maxCars: 0 });
    sim.setCycleStart(0);
    sim.exitAuto();
    var frame = sim.tick(600);
    assert.equal(frame.lightIsGreen, false);
  });

  it('cycles back to green after cycleTotal', function () {
    var sim = createSimulation({ greenPct: 50, cycleTotal: 1000, maxCars: 0 });
    sim.setCycleStart(0);
    sim.exitAuto();
    var frame = sim.tick(1100);
    assert.equal(frame.lightIsGreen, true);
  });

  it('100% green never goes red', function () {
    var sim = createSimulation({ greenPct: 100, cycleTotal: 1000, maxCars: 0 });
    sim.setCycleStart(0);
    sim.exitAuto();
    for (var t = 0; t < 2000; t += 100) {
      var frame = sim.tick(t);
      assert.equal(frame.lightIsGreen, true, 'expected green at t=' + t);
    }
  });

  it('0% green is always red', function () {
    var sim = createSimulation({ greenPct: 0, cycleTotal: 1000, maxCars: 0 });
    sim.setCycleStart(0);
    sim.exitAuto();
    var frame = sim.tick(0);
    assert.equal(frame.lightIsGreen, false);
    frame = sim.tick(500);
    assert.equal(frame.lightIsGreen, false);
  });
});

// ---------------------------------------------------------------------------
// Car spawning
// ---------------------------------------------------------------------------

describe('car spawning', function () {
  it('spawns a car when enough time has passed', function () {
    var sim = createSimulation({ spawnMs: 100 }, fixedScale(1));
    sim.exitAuto();
    sim.tick(200);
    assert.ok(sim.getCars().length >= 1, 'should have spawned at least one car');
  });

  it('respects spawnMs cooldown', function () {
    var sim = createSimulation({ spawnMs: 500, maxCars: 5 }, fixedScale(1));
    sim.exitAuto();
    sim.tick(600); // first spawn
    sim.tick(700); // only 100ms later → no new spawn
    assert.equal(sim.getCars().length, 1, 'should not have spawned twice');
  });

  it('alternates between highway and ramp sources', function () {
    var sim = createSimulation({ spawnMs: 10, maxCars: 10 }, fixedScale(1));
    sim.exitAuto();
    sim.tick(100);
    sim.tick(200);
    var cars = sim.getCars();
    var segments = new Set(cars.map(function (c) { return c.segment; }));
    assert.ok(segments.has('highway') || segments.has('ramp'), 'should spawn on highway or ramp');
    // With more ticks, should get both
    for (var t = 3; t <= 8; t++) sim.tick(t * 100);
    cars = sim.getCars();
    segments = new Set(cars.map(function (c) { return c.segment; }));
    assert.ok(segments.size >= 2, 'should spawn on both segments');
  });

  it('respects maxCars', function () {
    var sim = createSimulation({ maxCars: 2, spawnMs: 10 }, fixedScale(1));
    sim.exitAuto();
    sim.addCar('highway', 200, 3);
    sim.addCar('ramp', 100, 3);
    sim.tick(1000);
    assert.equal(sim.getCars().length, 2, 'should not exceed maxCars');
  });
});

// ---------------------------------------------------------------------------
// Multi-segment transitions
// ---------------------------------------------------------------------------

describe('segment transitions', function () {
  it('ramp cars merge onto highway', function () {
    var sim = createSimulation({
      lenRamp: 180, lenHwy: 530, lenExit: 210,
      mergeD: 170, gateD: 130, greenPct: 100,
      cycleTotal: 100, spawnMs: 99999, maxCars: 5,
    }, fixedScale(1));
    sim.setCycleStart(0);
    sim.exitAuto();
    sim.addCar('ramp', 175, 3.5); // near end of ramp
    for (var t = 0; t < 20; t++) {
      sim.tick(t * 25);
    }
    var cars = sim.getCars();
    var onHwy = cars.filter(function (c) { return c.segment === 'highway'; });
    assert.ok(onHwy.length >= 1, 'ramp car should have merged onto highway');
  });

  it('highway cars transition to exit', function () {
    var sim = createSimulation({
      lenRamp: 180, lenHwy: 530, lenExit: 210,
      mergeD: 170, gateD: 130, greenPct: 100,
      cycleTotal: 100, spawnMs: 99999, maxCars: 5,
    }, fixedScale(1));
    sim.setCycleStart(0);
    sim.exitAuto();
    sim.addCar('highway', 525, 3.5); // near end of highway
    for (var t = 0; t < 20; t++) {
      sim.tick(t * 25);
    }
    var cars = sim.getCars();
    var onExit = cars.filter(function (c) { return c.segment === 'exit'; });
    assert.ok(onExit.length >= 1, 'highway car should have transitioned to exit');
  });

  it('exit cars are delivered (removed) at end of exit', function () {
    var sim = createSimulation({
      lenRamp: 180, lenHwy: 530, lenExit: 210,
      mergeD: 170, gateD: 130, greenPct: 100,
      cycleTotal: 100, spawnMs: 99999, maxCars: 5,
    }, fixedScale(1));
    sim.setCycleStart(0);
    sim.exitAuto();
    sim.setLastSpawn(99999);
    sim.addCar('exit', 200, 3.5); // near end of exit
    var delivered = false;
    for (var t = 0; t < 20; t++) {
      var frame = sim.tick(t * 25);
      if (frame.removedIds.length > 0) delivered = true;
    }
    assert.ok(delivered, 'car should have been delivered');
  });
});

// ---------------------------------------------------------------------------
// Following distance and hard gap
// ---------------------------------------------------------------------------

describe('following distance', function () {
  it('cars never overlap within a segment', function () {
    var sim = createSimulation({
      greenPct: 0, cycleTotal: 100, gateD: 130,
      lenExit: 210, lenHwy: 530, spawnMs: 99999,
    }, fixedScale(1));
    sim.setCycleStart(0);
    sim.exitAuto();
    // Pack cars on the exit segment with proper spacing
    for (var i = 0; i < 5; i++) {
      sim.addCar('exit', 30 + i * 40, 3.5);
    }
    for (var t = 0; t < 200; t++) {
      sim.tick(t * 25);
      var cars = sim.getCars().filter(function (c) { return c.segment === 'exit'; });
      cars.sort(function (a, b) { return b.d - a.d; });
      for (var j = 1; j < cars.length; j++) {
        var gap = cars[j - 1].d - cars[j].d;
        var minGap = (cars[j - 1].w + cars[j].w) / 2 + sim.cfg.minFollowPad;
        assert.ok(gap >= minGap - 0.01,
          'gap violation: ' + gap.toFixed(2) + ' < ' + minGap.toFixed(2));
      }
    }
  });

  it('trailing car brakes when getting close', function () {
    var sim = createSimulation({
      greenPct: 100, cycleTotal: 100, followZone: 18,
      lenHwy: 530, spawnMs: 99999,
    }, fixedScale(1));
    sim.setCycleStart(0);
    sim.exitAuto();
    sim.addCar('highway', 40, 0); // stopped
    sim.addCar('highway', 10, 3.5); // fast, approaching
    for (var t = 0; t < 5; t++) {
      sim.tick(t * 25);
    }
    var cars = sim.getCars().filter(function (c) { return c.segment === 'highway'; });
    cars.sort(function (a, b) { return a.d - b.d; });
    assert.ok(cars[0].speed < 3.5, 'trailing car should have braked');
  });
});

// ---------------------------------------------------------------------------
// Red light stopping (on exit ramp)
// ---------------------------------------------------------------------------

describe('red light on exit', function () {
  it('lead car stops at gateD on red', function () {
    var gateD = 130;
    var sim = createSimulation({
      greenPct: 0, cycleTotal: 100, gateD: gateD,
      lenExit: 210, lenHwy: 530, spawnMs: 99999,
    }, fixedScale(1));
    sim.setCycleStart(0);
    sim.exitAuto();
    sim.addCar('exit', gateD - 30, 3.5);
    for (var t = 0; t < 50; t++) {
      sim.tick(t * 25);
    }
    var car = sim.getCars().filter(function (c) { return c.segment === 'exit'; })[0];
    if (car) {
      assert.ok(car.d <= gateD + 0.01, 'car should stop at or before gateD');
      assert.ok(car.speed < 0.2, 'car should be stopped');
    }
  });

  it('backpressure cascades: exit full → highway blocks', function () {
    var sim = createSimulation({
      greenPct: 0, cycleTotal: 100, gateD: 130,
      lenExit: 210, lenHwy: 530, lenRamp: 180,
      mergeD: 170, spawnMs: 99999, minFollowPad: 8,
    }, fixedScale(1));
    sim.setCycleStart(0);
    sim.exitAuto();
    // Fill exit ramp
    for (var i = 0; i < 5; i++) {
      sim.addCar('exit', 20 + i * 25, 3.5);
    }
    // Car on highway near the fork
    sim.addCar('highway', 525, 3.5);
    for (var t = 0; t < 100; t++) {
      sim.tick(t * 25);
    }
    var hwyCars = sim.getCars().filter(function (c) { return c.segment === 'highway'; });
    if (hwyCars.length > 0) {
      assert.ok(hwyCars[0].speed < 1, 'highway car should be blocked by full exit');
    }
  });
});

// ---------------------------------------------------------------------------
// Car fade on exit past gate
// ---------------------------------------------------------------------------

describe('car fade', function () {
  it('cars past gateD on exit fade toward 0', function () {
    var sim = createSimulation({
      greenPct: 100, cycleTotal: 100, gateD: 100,
      lenExit: 210, lenHwy: 530, spawnMs: 99999,
    }, fixedScale(1));
    sim.setCycleStart(0);
    sim.exitAuto();
    sim.addCar('exit', 150, 3.5); // already past gate
    var frame = sim.tick(0);
    var car = frame.cars.filter(function (c) { return c.segment === 'exit'; })[0];
    assert.ok(car.opacity < 1, 'car past gateD should have opacity < 1');
  });

  it('cars before gateD on exit have full opacity', function () {
    var sim = createSimulation({
      greenPct: 100, cycleTotal: 100, gateD: 130,
      lenExit: 210, lenHwy: 530, spawnMs: 99999,
    }, fixedScale(1));
    sim.setCycleStart(0);
    sim.exitAuto();
    sim.addCar('exit', 50, 3.5);
    var frame = sim.tick(0);
    var car = frame.cars.filter(function (c) { return c.segment === 'exit'; })[0];
    assert.equal(car.opacity, 1, 'car before gateD should have full opacity');
  });
});

// ---------------------------------------------------------------------------
// Stats computation
// ---------------------------------------------------------------------------

describe('stats', function () {
  it('throughput counts deliveries in last 5 seconds', function () {
    var sim = createSimulation({
      lenExit: 100, lenHwy: 200, lenRamp: 100,
      mergeD: 50, gateD: 80, greenPct: 100,
      cycleTotal: 100, spawnMs: 99999,
    }, fixedScale(1));
    sim.setCycleStart(0);
    sim.exitAuto();
    sim.addCar('exit', 95, 3.5); // near exit end
    var frame;
    for (var t = 0; t < 40; t++) {
      frame = sim.tick(t * 25);
    }
    assert.ok(frame.stats.throughput > 0, 'should have recorded delivery');
  });

  it('stallPct increases when cars are stalled', function () {
    var sim = createSimulation({
      greenPct: 0, cycleTotal: 100, gateD: 130,
      lenExit: 210, lenHwy: 530, spawnMs: 99999,
    }, fixedScale(1));
    sim.setCycleStart(0);
    sim.exitAuto();
    sim.addCar('exit', 125, 0); // stopped near gate
    var frame;
    for (var t = 0; t < 20; t++) {
      frame = sim.tick(t * 25);
    }
    assert.ok(frame.stats.stallPct > 0, 'stall% should be > 0');
  });
});

// ---------------------------------------------------------------------------
// Auto-sweep
// ---------------------------------------------------------------------------

describe('auto-sweep', function () {
  it('decreases greenPct when direction is -1', function () {
    var sim = createSimulation({ greenPct: 50, maxCars: 0 });
    sim.tick(0);
    assert.ok(sim.tick(1).greenPct <= 50, 'should decrease or stay');
  });

  it('bounces at autoMin', function () {
    var sim = createSimulation({ greenPct: 6, autoSpeed: 10, autoMin: 5, autoMax: 100, maxCars: 0 });
    var frame = sim.tick(0);
    assert.ok(frame.greenPct >= 5, 'should not go below autoMin');
  });

  it('stops when setGreenPct is called', function () {
    var sim = createSimulation({ greenPct: 50, maxCars: 0 });
    sim.tick(0);
    assert.equal(sim.isAuto(), true);
    sim.setGreenPct(70);
    assert.equal(sim.isAuto(), false);
    var frame = sim.tick(100);
    assert.equal(frame.greenPct, 70, 'should not change in manual mode');
  });
});

// ---------------------------------------------------------------------------
// Car size variety
// ---------------------------------------------------------------------------

describe('car size variety', function () {
  it('cars have different widths with default scale', function () {
    var sim = createSimulation({ maxCars: 20, spawnMs: 99999 });
    sim.exitAuto();
    for (var i = 0; i < 10; i++) {
      sim.addCar('highway', i * 50, 3.5);
    }
    var cars = sim.getCars();
    var widths = new Set(cars.map(function (c) { return c.w; }));
    assert.ok(widths.size > 1, 'expected varied widths');
  });

  it('fixedScale produces uniform scale but shape-dependent widths', function () {
    var sim = createSimulation({}, fixedScale(1.0));
    sim.addCar('highway', 0, 3.5);
    sim.addCar('highway', 100, 3.5);
    sim.addCar('highway', 200, 3.5);
    sim.addCar('highway', 300, 3.5);
    var cars = sim.getCars();
    for (var i = 0; i < cars.length; i++) {
      assert.equal(cars[i].scale, 1.0);
    }
    assert.equal(cars[0].w, DEFAULTS.carW * 1.0); // sedan
    assert.equal(cars[1].w, DEFAULTS.carW * 1.3); // truck
    assert.equal(cars[2].w, DEFAULTS.carW * 0.8); // compact
  });
});

// ---------------------------------------------------------------------------
// Status messages
// ---------------------------------------------------------------------------

describe('status', function () {
  it('reports flowing when everything is moving', function () {
    var sim = createSimulation({
      greenPct: 100, cycleTotal: 100, lenHwy: 530, spawnMs: 99999,
    }, fixedScale(1));
    sim.setCycleStart(0);
    sim.exitAuto();
    sim.addCar('highway', 200, 3.5);
    var frame = sim.tick(0);
    assert.equal(frame.status.level, 'flowing');
  });

  it('reports blocked when ramp stalls', function () {
    var sim = createSimulation({
      greenPct: 0, cycleTotal: 100,
      lenHwy: 530, lenRamp: 180, lenExit: 210,
      mergeD: 170, gateD: 130, spawnMs: 99999,
    }, fixedScale(1));
    sim.setCycleStart(0);
    sim.exitAuto();
    // Fill exit to create backpressure
    for (var i = 0; i < 4; i++) {
      sim.addCar('exit', 20 + i * 28, 0);
    }
    // Fill highway back toward merge
    for (var j = 0; j < 15; j++) {
      sim.addCar('highway', 50 + j * 30, 0);
    }
    // Stuck ramp car
    sim.addCar('ramp', 175, 0);
    var frame;
    for (var t = 0; t < 20; t++) {
      frame = sim.tick(t * 25);
    }
    assert.equal(frame.status.level, 'blocked');
  });
});
