// Track 3 cars over 100 frames to check for teleporting/jerkiness
async function check(page) {
  // Wait for animation to start
  await page.waitForTimeout(2000);
  
  const data = await page.evaluate(() => {
    return new Promise(resolve => {
      const results = [];
      let frame = 0;
      const interval = setInterval(() => {
        const cars = document.querySelectorAll('#hw-cars > *');
        const snapshot = [];
        cars.forEach(c => {
          const t = c.getAttribute('transform');
          if (!t) return;
          const m = t.match(/translate\(([\d.]+),\s*([\d.]+)\)/);
          if (m) {
            const id = c.getAttribute('data-car-id') || '?';
            snapshot.push({ id, x: +m[1], y: +m[2] });
          }
        });
        results.push({ f: frame, cars: snapshot });
        frame++;
        if (frame >= 60) {
          clearInterval(interval);
          
          // Analyze: find max frame-to-frame jumps per car
          const carJumps = {};
          for (let i = 1; i < results.length; i++) {
            const prev = {};
            results[i-1].cars.forEach(c => prev[c.id] = c);
            results[i].cars.forEach(c => {
              if (prev[c.id]) {
                const dx = c.x - prev[c.id].x;
                const dy = c.y - prev[c.id].y;
                const dist = Math.sqrt(dx*dx + dy*dy);
                if (!carJumps[c.id]) carJumps[c.id] = [];
                if (dist > 5) carJumps[c.id].push({ f: i, dist: +dist.toFixed(1), dx: +dx.toFixed(1), dy: +dy.toFixed(1) });
              }
            });
          }
          resolve(JSON.stringify({ bigJumps: carJumps }, null, 2));
        }
      }, 25);
    });
  });
  return data;
}
check(page);
