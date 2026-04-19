/**
 * Highway backpressure simulation — pure logic, no DOM.
 *
 * Multi-route model:
 *   highway: cars enter from left, travel right to the fork
 *   ramp:    cars enter from bottom-left, merge into highway at mergeD
 *   exit:    all cars leave highway via exit ramp (has traffic light)
 *
 * The highway visually continues right past the fork but all simulated
 * cars take the exit ramp. When the light is red, the exit backs up →
 * highway backs up → on-ramp stalls. Cars fade out past the light.
 */

export var DEFAULTS = {
  lenRamp: 180,
  lenHwy: 530,
  lenExit: 210,
  mergeD: 170,
  gateD: 130,
  carW: 20,
  minFollowPad: 8,
  speedMax: 3.5,
  spawnMs: 420,
  cycleTotal: 3000,
  greenPct: 80,
  maxCars: 35,
  lerpRate: 0.25,
  brakeRate: 0.3,
  followZone: 18,
  autoSpeed: 0.15,
  autoMin: 5,
  autoMax: 100,
  scaleMin: 0.7,
  scaleMax: 1.3,
};

var SHAPES = ['sedan', 'truck', 'compact', 'van'];
var SHAPE_W = { sedan: 1, truck: 1.3, compact: 0.8, van: 1 };

export function fixedScale(s) {
  return function () { return s; };
}

export function createSimulation(overrides, scaleFn) {
  var cfg = {};
  var k;
  for (k in DEFAULTS) cfg[k] = DEFAULTS[k];
  if (overrides) for (k in overrides) cfg[k] = overrides[k];

  var getScale = scaleFn || function () {
    return cfg.scaleMin + Math.random() * (cfg.scaleMax - cfg.scaleMin);
  };

  var nextId = 0;
  var carsRamp = [];
  var carsHwy = [];
  var carsExit = [];

  var lightIsGreen = true;
  var greenPct = cfg.greenPct;
  var cycleStart = 0;
  var lastSpawn = -Infinity;
  var spawnSrc = 0; // alternates 0=hwy, 1=ramp

  var autoMode = true;
  var autoVal = greenPct;
  var autoDir = -1;

  var deliveries = [];
  var stalledFrames = 0;
  var totalFrames = 0;

  // ---- helpers ----

  function makeCar(segment, d, speed, scale) {
    var s = scale != null ? scale : getScale();
    var shape = SHAPES[nextId % SHAPES.length];
    var shapeMul = SHAPE_W[shape] || 1;
    var car = {
      id: nextId++,
      segment: segment,
      d: d != null ? d : 0,
      speed: speed != null ? speed : cfg.speedMax,
      scale: s,
      w: cfg.carW * s * shapeMul,
      shape: shape,
      color: 'flow',
      opacity: 1,
    };
    car.color = carColor(car);
    return car;
  }

  function carColor(car) {
    if (car.speed < 0.15) return 'stop';
    if (car.speed < cfg.speedMax * 0.35) return 'slow';
    return 'flow';
  }

  function minGap(a, b) {
    return (a.w + b.w) / 2 + cfg.minFollowPad;
  }

  function allCars() {
    return carsRamp.concat(carsHwy, carsExit);
  }

  // ---- light ----

  function tickLight(now) {
    if (greenPct >= 100) { lightIsGreen = true; return; }
    var elapsed = (now - cycleStart) % cfg.cycleTotal;
    lightIsGreen = elapsed < cfg.cycleTotal * greenPct / 100;
  }

  function tickAuto() {
    if (!autoMode) return;
    autoVal += cfg.autoSpeed * autoDir;
    if (autoVal <= cfg.autoMin) { autoVal = cfg.autoMin; autoDir = 1; }
    if (autoVal >= cfg.autoMax) { autoVal = cfg.autoMax; autoDir = -1; }
    greenPct = Math.round(autoVal);
  }

  // ---- spawning ----

  function hasSpawnSpace(cars) {
    for (var j = 0; j < cars.length; j++) {
      if (cars[j].d < cfg.minFollowPad + cars[j].w) return false;
    }
    return true;
  }

  function trySpawn(now) {
    if (allCars().length >= cfg.maxCars) return null;
    if (now - lastSpawn < cfg.spawnMs) return null;

    var src = spawnSrc;
    spawnSrc = 1 - spawnSrc;
    var car = null;

    if (src === 0 && hasSpawnSpace(carsHwy)) {
      car = makeCar('highway', 0);
      carsHwy.push(car);
    } else if (src === 1 && hasSpawnSpace(carsRamp)) {
      car = makeCar('ramp', 0);
      carsRamp.push(car);
    } else if (src === 0 && hasSpawnSpace(carsRamp)) {
      car = makeCar('ramp', 0);
      carsRamp.push(car);
    } else if (src === 1 && hasSpawnSpace(carsHwy)) {
      car = makeCar('highway', 0);
      carsHwy.push(car);
    }

    if (car) lastSpawn = now;
    return car;
  }

  // ---- movement within a segment ----

  function moveSegment(cars, segLen, opts) {
    cars.sort(function (a, b) { return b.d - a.d; });

    for (var i = 0; i < cars.length; i++) {
      var car = cars[i];
      var target = cfg.speedMax;

      // Gate (light) — only on exit
      if (opts.hasGate && !lightIsGreen && car.d <= cfg.gateD && car.d > cfg.gateD - 35) {
        var someoneCloser = (i > 0 && cars[i - 1].d <= cfg.gateD && cars[i - 1].d > car.d);
        if (!someoneCloser) {
          target = 0;
          if (car.d + car.speed > cfg.gateD) {
            car.d = cfg.gateD;
            car.speed = 0;
          }
        }
      }

      // End-of-segment blocking
      if (opts.endBlocked && car.d >= segLen - 8) {
        target = 0;
        if (car.d > segLen) { car.d = segLen; car.speed = 0; }
      }

      // Following distance
      if (i > 0) {
        var ahead = cars[i - 1];
        var gap = ahead.d - car.d;
        var mg = minGap(car, ahead);
        if (gap < mg + cfg.followZone) {
          target = Math.min(target, Math.max(0, (gap - mg) * cfg.brakeRate));
        }
      }

      // Smooth speed
      car.speed += (target - car.speed) * cfg.lerpRate;
      if (car.speed < 0.03) car.speed = 0;
      car.d += car.speed;

      // Hard gap
      if (i > 0) {
        var ahead2 = cars[i - 1];
        var mg2 = minGap(car, ahead2);
        if (ahead2.d - car.d < mg2) {
          car.d = ahead2.d - mg2;
          car.speed = Math.min(car.speed, ahead2.speed);
        }
      }

      if (car.d < 0) car.d = 0;

      // Fade past gate on exit
      if (opts.fade && car.d > cfg.gateD) {
        car.opacity = Math.max(0, 1 - (car.d - cfg.gateD) / (segLen - cfg.gateD));
      } else {
        car.opacity = 1;
      }

      car.color = carColor(car);
    }
  }

  // ---- transitions ----

  function canEnterExit() {
    for (var j = 0; j < carsExit.length; j++) {
      if (carsExit[j].d < cfg.minFollowPad + carsExit[j].w + cfg.carW) return false;
    }
    return true;
  }

  function canMergeAt(d, w) {
    for (var i = 0; i < carsHwy.length; i++) {
      var hw = carsHwy[i];
      var needed = (hw.w + w) / 2 + cfg.minFollowPad;
      if (Math.abs(hw.d - d) < needed) return false;
    }
    return true;
  }

  function tryHwyToExit() {
    if (carsHwy.length === 0) return;
    carsHwy.sort(function (a, b) { return b.d - a.d; });
    var front = carsHwy[0];
    if (front.d < cfg.lenHwy - 3) return;
    if (canEnterExit()) {
      carsHwy.splice(0, 1);
      front.segment = 'exit';
      front.d = Math.max(0, front.d - cfg.lenHwy);
      carsExit.push(front);
    } else {
      front.d = cfg.lenHwy;
      front.speed = 0;
    }
  }

  function tryRampMerge() {
    if (carsRamp.length === 0) return;
    carsRamp.sort(function (a, b) { return b.d - a.d; });
    var front = carsRamp[0];
    if (front.d < cfg.lenRamp - 3) return;
    if (canMergeAt(cfg.mergeD, front.w)) {
      carsRamp.splice(0, 1);
      front.segment = 'highway';
      front.d = cfg.mergeD;
      carsHwy.push(front);
    } else {
      front.d = cfg.lenRamp;
      front.speed = 0;
    }
  }

  // ---- main tick ----

  function tick(now) {
    totalFrames++;
    tickAuto();
    tickLight(now);

    var removedIds = [];

    // 1. Remove delivered
    for (var i = carsExit.length - 1; i >= 0; i--) {
      if (carsExit[i].d >= cfg.lenExit || carsExit[i].opacity <= 0.01) {
        removedIds.push(carsExit[i].id);
        deliveries.push(now);
        carsExit.splice(i, 1);
      }
    }

    // 2. Move exit (downstream first — frees space)
    moveSegment(carsExit, cfg.lenExit, { hasGate: true, fade: true });

    // 3. Highway → exit
    tryHwyToExit();

    // 4. Move highway
    var exitFull = !canEnterExit();
    moveSegment(carsHwy, cfg.lenHwy, { endBlocked: exitFull });

    // 5. Try transition again (car may have moved to end this tick)
    tryHwyToExit();

    // 6. Ramp → highway
    tryRampMerge();

    // 7. Move ramp
    var mergeFull = !canMergeAt(cfg.mergeD, cfg.carW);
    moveSegment(carsRamp, cfg.lenRamp, { endBlocked: mergeFull });

    // 8. Try merge again
    tryRampMerge();

    // 9. Spawn
    trySpawn(now);

    // Stats
    var cutoff = now - 5000;
    while (deliveries.length > 0 && deliveries[0] < cutoff) deliveries.shift();
    var throughput = Math.round(deliveries.length * 12);

    var all = allCars();
    var stalledCount = 0;
    var rampBlocked = false;
    for (var s = 0; s < all.length; s++) {
      if (all[s].speed < 0.15) {
        stalledCount++;
        if (all[s].segment === 'ramp' && all[s].d >= cfg.lenRamp - 5) rampBlocked = true;
        if (all[s].segment === 'highway' && all[s].d < 10) rampBlocked = true;
      }
    }
    if (stalledCount > 0) stalledFrames++;

    var stallPct = totalFrames > 0 ? Math.round(stalledFrames / totalFrames * 100) : 0;

    var status;
    if (rampBlocked) {
      status = { level: 'blocked', msg: 'Traffic backed up to the on-ramp \u2014 no new cars can enter' };
    } else if (stalledCount > all.length * 0.4) {
      status = { level: 'congested', msg: 'Highway congested \u2014 cars queuing behind the light' };
    } else {
      status = { level: 'flowing', msg: 'Flowing \u2014 traffic moving freely' };
    }

    return {
      cars: all,
      removedIds: removedIds,
      lightIsGreen: lightIsGreen,
      greenPct: greenPct,
      autoMode: autoMode,
      stats: { throughput: throughput, queued: carsHwy.length + carsExit.length, stallPct: stallPct },
      status: status,
    };
  }

  return {
    tick: tick,
    getCars: allCars,
    setGreenPct: function (v) {
      greenPct = v;
      autoMode = false;
      stalledFrames = 0;
      totalFrames = 0;
    },
    exitAuto: function () { autoMode = false; },
    isAuto: function () { return autoMode; },
    setCycleStart: function (t) { cycleStart = t; },
    setLastSpawn: function (t) { lastSpawn = t; },
    addCar: function (segment, d, speed, scale) {
      var car = makeCar(segment, d, speed, scale);
      if (segment === 'ramp') carsRamp.push(car);
      else if (segment === 'highway') carsHwy.push(car);
      else if (segment === 'exit') carsExit.push(car);
      return car;
    },
    cfg: cfg,
  };
}
