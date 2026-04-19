/**
 * Highway render state — pure interpolation logic, no DOM.
 *
 * Each car accumulates a queue of screen-space waypoints {x, y, opacity, color}.
 * Every render frame, cars advance along their waypoint chain using a 1D velocity
 * chase. This eliminates segment-boundary teleportation because waypoints are
 * sampled from SVG paths and stitched together in screen space — there are no
 * coordinate-system jumps.
 *
 * Usage:
 *   const rs = createRenderState({ fadeInFrames: 12, maxSpeed: 4, ... });
 *   rs.addCar(id, x, y, angle, initSpeed);
 *   // per sim tick:
 *   rs.pushTarget(id, x, y, opacity, color);
 *   // per render frame:
 *   rs.step();
 *   const vis = rs.getCar(id); // { x, y, angle, opacity, color, fadeAge }
 */

export const RENDER_DEFAULTS = {
  fadeInFrames: 12,
  maxSpeed: 4,
  accel: 0.5,
  decelDist: 30,
  snapDist: 0.5,
  angleRate: 0.25,
  minSep: 18, // minimum screen-space distance between car centers
};

export function createRenderState(overrides) {
  const cfg = {};
  for (const k in RENDER_DEFAULTS) cfg[k] = RENDER_DEFAULTS[k];
  if (overrides) for (const k in overrides) cfg[k] = overrides[k];

  const cars = {}; // id → internal state

  function addCar(id, x, y, angle, initSpeed) {
    cars[id] = {
      waypoints: [],    // [{x, y, opacity, color}]
      x: x,
      y: y,
      speed: initSpeed != null ? initSpeed : 0,
      angle: angle || 0,
      fadeAge: 0,
      color: 'flow',
      opacity: 1,
    };
  }

  function removeCar(id) {
    delete cars[id];
  }

  function pushTarget(id, x, y, opacity, color) {
    const car = cars[id];
    if (!car) return;
    car.waypoints.push({
      x: x,
      y: y,
      opacity: opacity != null ? opacity : 1,
      color: color || 'flow',
    });
  }

  // Compute how far `car` can move toward its first waypoint before
  // colliding with another car that's roughly ahead of it.
  // Returns the max movement distance (pixels this frame).
  function clearanceAhead(car, id) {
    if (car.waypoints.length === 0) return Infinity;
    const wp = car.waypoints[0];
    const dx = wp.x - car.x;
    const dy = wp.y - car.y;
    const dist = Math.sqrt(dx * dx + dy * dy);
    if (dist < 0.01) return Infinity;

    const dirX = dx / dist;
    const dirY = dy / dist;

    let minAhead = Infinity;
    for (const otherId in cars) {
      if (otherId === id) continue;
      const other = cars[otherId];
      const odx = other.x - car.x;
      const ody = other.y - car.y;
      // Project onto our movement direction
      const ahead = odx * dirX + ody * dirY;
      if (ahead <= 0) continue; // behind us — ignore
      // Perpendicular distance — only care if roughly in our lane
      const perpSq = (odx * odx + ody * ody) - ahead * ahead;
      if (perpSq > cfg.minSep * cfg.minSep) continue; // different lane
      if (ahead < minAhead) minAhead = ahead;
    }
    // Usable gap = distance to nearest car ahead minus the separation buffer
    return Math.max(0, minAhead - cfg.minSep);
  }

  function stepCar(car, maxMove) {
    if (car.waypoints.length === 0) {
      car.speed = Math.max(0, car.speed - cfg.accel * 2);
      return;
    }

    const wp = car.waypoints[0];
    const dx = wp.x - car.x;
    const dy = wp.y - car.y;
    const dist = Math.sqrt(dx * dx + dy * dy);

    if (dist < cfg.snapDist) {
      car.x = wp.x;
      car.y = wp.y;
      car.color = wp.color;
      car.opacity = wp.opacity;
      car.waypoints.shift();
      if (car.waypoints.length > 0) return;
      car.speed = 0;
      return;
    }

    // Total remaining path distance through all waypoints
    let totalDist = dist;
    let prevX = wp.x, prevY = wp.y;
    for (let i = 1; i < car.waypoints.length; i++) {
      const nxt = car.waypoints[i];
      const segDx = nxt.x - prevX;
      const segDy = nxt.y - prevY;
      totalDist += Math.sqrt(segDx * segDx + segDy * segDy);
      prevX = nxt.x;
      prevY = nxt.y;
    }

    // Desired speed — decelerate for both path end AND clearance
    const pathDesired = Math.min(
      cfg.maxSpeed,
      (totalDist / cfg.decelDist) * cfg.maxSpeed,
    );
    // Also decelerate to respect clearance ahead
    const clearDesired = maxMove < Infinity
      ? Math.min(cfg.maxSpeed, (maxMove / cfg.decelDist) * cfg.maxSpeed)
      : cfg.maxSpeed;
    const desired = Math.min(pathDesired, clearDesired);

    if (car.speed < desired) {
      car.speed = Math.min(car.speed + cfg.accel, desired);
    } else {
      car.speed = Math.max(car.speed - cfg.accel * 2, desired);
    }
    car.speed = Math.max(0, car.speed);

    // Hard cap: never move more than available clearance
    let moveD = Math.min(car.speed, maxMove);

    const dirX = dx / dist;
    const dirY = dy / dist;

    if (moveD >= dist) {
      car.x = wp.x;
      car.y = wp.y;
      car.color = wp.color;
      car.opacity = wp.opacity;
      car.waypoints.shift();
      const remaining = moveD - dist;
      if (remaining > 0 && car.waypoints.length > 0) {
        const nxt = car.waypoints[0];
        const ndx = nxt.x - car.x;
        const ndy = nxt.y - car.y;
        const ndist = Math.sqrt(ndx * ndx + ndy * ndy);
        if (ndist > 0.01) {
          const frac = Math.min(remaining / ndist, 1);
          car.x += ndx * frac;
          car.y += ndy * frac;
        }
      }
    } else {
      car.x += dirX * moveD;
      car.y += dirY * moveD;
    }

    // Angle follows movement direction
    if (moveD > 0.1) {
      const moveAngle = Math.atan2(dirY, dirX) * 180 / Math.PI;
      let da = moveAngle - car.angle;
      while (da > 180) da -= 360;
      while (da < -180) da += 360;
      car.angle += da * cfg.angleRate;
    }
  }

  function step() {
    const ids = Object.keys(cars);
    for (let i = 0; i < ids.length; i++) {
      const id = ids[i];
      const car = cars[id];
      const maxMove = clearanceAhead(car, id);
      stepCar(car, maxMove);
      car.fadeAge = Math.min(1, car.fadeAge + 1 / cfg.fadeInFrames);
    }
  }

  function getCar(id) {
    const car = cars[id];
    if (!car) return null;

    const color = car.fadeAge < 1 ? 'flow' : car.color;
    const opacity = car.fadeAge * car.opacity;

    return {
      x: car.x,
      y: car.y,
      angle: car.angle,
      opacity: opacity,
      color: color,
      fadeAge: car.fadeAge,
    };
  }

  function getAllCars() {
    const result = [];
    for (const id in cars) {
      const vis = getCar(id);
      if (vis) {
        vis.id = id;
        result.push(vis);
      }
    }
    return result;
  }

  function hasCar(id) {
    return id in cars;
  }

  return {
    addCar: addCar,
    removeCar: removeCar,
    pushTarget: pushTarget,
    step: step,
    getCar: getCar,
    getAllCars: getAllCars,
    hasCar: hasCar,
  };
}
