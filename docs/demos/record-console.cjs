// Record the live, unmodified Jiandu UI. Requires Playwright and Chromium.
const path = require('node:path');
const { chromium } = require(process.env.PLAYWRIGHT_MODULE || 'playwright');
(async () => {
  const out = path.resolve(__dirname);
  const browser = await chromium.launch({
    executablePath: process.env.CHROMIUM_BIN || '/usr/bin/chromium',
    args: ['--no-sandbox'],
  });
  const context = await browser.newContext({
    viewport: { width: 1120, height: 800 },
    recordVideo: { dir: process.env.VIDEO_DIR || '/tmp/jiandu-demo-video', size: { width: 1120, height: 800 } },
  });
  const page = await context.newPage();
  await page.goto(process.env.JIANDU_URL || 'http://127.0.0.1:9123/');
  await page.locator('[data-scope="project"]').waitFor();
  await page.waitForTimeout(1800);
  await page.locator('[data-scope="project"]').click();
  await page.getByText('Demo: release checklist', {exact: true}).waitFor();
  await page.waitForTimeout(2200);
  await page.locator('#query').pressSequentially('release checklist', {delay: 100});
  await page.locator('#query').press('Enter');
  await page.waitForTimeout(2300);
  await page.getByText('Demo: release checklist', {exact: true}).click();
  await page.locator('#detail').getByText('Demo: release checklist', {exact: true}).waitFor();
  await page.waitForTimeout(2000);
  await page.mouse.move(1000, 650);
  await page.mouse.wheel(0, 300);
  await page.waitForTimeout(4000);
  await page.screenshot({path: path.join(out, 'memory-console.png')});
  const video = page.video();
  await context.close();
  console.log('VIDEO=' + await video.path());
  await browser.close();
})();
