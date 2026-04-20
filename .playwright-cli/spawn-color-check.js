async (page) => {
  const seen = new Set();
  const spawnColors = [];
  for (let i = 0; i < 40; i++) {
    const cars = await page.$$('[data-car-id]');
    for (const car of cars) {
      const id = await car.getAttribute('data-car-id');
      if (!seen.has(id)) {
        seen.add(id);
        const fill = await car.evaluate(el => {
          const rect = el.querySelector('rect,circle,polygon');
          return rect ? getComputedStyle(rect).fill : 'unknown';
        });
        spawnColors.push({ id, fill });
      }
    }
    await page.waitForTimeout(200);
  }
  return JSON.stringify({ totalSpawns: spawnColors.length, colors: spawnColors });
}
