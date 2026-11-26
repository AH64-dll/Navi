"""Pure service rendering tests, no installation or systemctl calls."""
import importlib.util
from pathlib import Path
import unittest
spec=importlib.util.spec_from_file_location('navi_installer',Path(__file__).with_name('install.py'))
installer=importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)

class InstallTests(unittest.TestCase):
 def test_optional_owner_config_is_loaded_after_defaults_before_exec(self):
  text=installer.station_unit('/project/apps/navi','/private/bin/navid','/private/bin')
  self.assertEqual(text.count('EnvironmentFile='),1)
  self.assertIn('EnvironmentFile=-%h/.config/navi/runtime.env\n',text)
  self.assertLess(text.index('Environment=NAVI_USER_PATH_DEFAULT='),text.index('EnvironmentFile='))
  self.assertLess(text.index('EnvironmentFile='),text.index('ExecStart='))
  self.assertIn('ExecStart=/private/bin/navid serve\n',text)
 def test_rendering_does_not_copy_model_or_credentials(self):
  text=installer.station_unit('/project','/bin/navid','/bin')
  self.assertNotIn('NAVI_STATION_MODEL=',text)
  self.assertNotIn('API_KEY',text)
  self.assertIn('KillMode=control-group',text)
  self.assertIn('Restart=on-failure',text)
if __name__=='__main__':unittest.main()
