# A window the GUI tests drive: a known block pattern, a focused text box, and one line of standard output per thing it
# receives. Windows PowerShell 5.1 (WinForms through Add-Type); never touches anything but its own window.
#
#   powershell -NoProfile -STA -File gui_probe.ps1 -X 100 -Y 100 [-Seconds 120]
#
# Output (physical screen pixels):
#   ready X Y W H      the pattern's top-left and size, once the window is up
#   focused / unfocused  the window gained or lost the foreground
#   mouse BUTTON X Y   a button went down over the window
#   wheel V|H DELTA    the wheel turned (120 a notch; positive is away from the person, or to the right)
#   char U+XXXX        a character reached the text box
#   down KEY ctrl=0 shift=0 alt=0 / up KEY   a key went down or up in the text box
#   text ESCAPED       the text box's content, URL-escaped, after each change
#   bye                the window closed
param(
    [int]$X = 100,
    [int]$Y = 100,
    [int]$Seconds = 120
)

$source = @'
using System;
using System.Drawing;
using System.Runtime.InteropServices;
using System.Windows.Forms;

public static class Probe
{
    // The pattern: CELLS x CELLS cells of CELL pixels, each a solid colour the test computes the same way.
    public const int Cells = 4;
    public const int Cell = 32;

    [DllImport("user32.dll")]
    static extern bool SetProcessDpiAwarenessContext(IntPtr value);

    static void Say(string line)
    {
        Console.Out.WriteLine(line);
        Console.Out.Flush();
    }

    class Wheel : IMessageFilter
    {
        public bool PreFilterMessage(ref Message m)
        {
            if (m.Msg == 0x020A || m.Msg == 0x020E)
            {
                long w = m.WParam.ToInt64();
                short delta = unchecked((short)((w >> 16) & 0xFFFF));
                Say("wheel " + (m.Msg == 0x020A ? "V " : "H ") + delta);
            }
            return false;
        }
    }

    class Pattern : Panel
    {
        public Pattern()
        {
            DoubleBuffered = false;
            SetStyle(ControlStyles.AllPaintingInWmPaint | ControlStyles.UserPaint, true);
        }

        protected override void OnPaint(PaintEventArgs e)
        {
            for (int cy = 0; cy < Cells; cy++)
            {
                for (int cx = 0; cx < Cells; cx++)
                {
                    Color color = Color.FromArgb(30 + 50 * cx, 30 + 50 * cy, 60 + 30 * ((cx + cy) % 4));
                    using (SolidBrush brush = new SolidBrush(color))
                    {
                        e.Graphics.FillRectangle(brush, cx * Cell, cy * Cell, Cell, Cell);
                    }
                }
            }
        }
    }

    public static void Run(int x, int y, int seconds)
    {
        SetProcessDpiAwarenessContext(new IntPtr(-4));
        Application.EnableVisualStyles();
        Form form = new Form();
        form.FormBorderStyle = FormBorderStyle.None;
        form.StartPosition = FormStartPosition.Manual;
        form.ShowInTaskbar = false;
        form.TopMost = true;
        int side = Cells * Cell;
        form.Location = new Point(x, y);
        form.ClientSize = new Size(side, side + 28);
        Pattern pattern = new Pattern();
        pattern.SetBounds(0, 0, side, side);
        TextBox box = new TextBox();
        box.SetBounds(0, side, side, 28);
        box.Font = new Font("Segoe UI", 11f);
        form.Controls.Add(pattern);
        form.Controls.Add(box);
        form.ActiveControl = box;

        form.Activated += delegate { Say("focused"); };
        form.Deactivate += delegate { Say("unfocused"); };
        MouseEventHandler down = delegate (object s, MouseEventArgs e)
        {
            Point p = Cursor.Position;
            Say("mouse " + e.Button.ToString().ToLower() + " " + p.X + " " + p.Y);
        };
        pattern.MouseDown += down;
        form.MouseDown += down;
        box.MouseDown += down;
        box.KeyPress += delegate (object s, KeyPressEventArgs e)
        {
            Say("char U+" + ((int)e.KeyChar).ToString("X4"));
        };
        box.KeyDown += delegate (object s, KeyEventArgs e)
        {
            Say("down " + e.KeyCode + " ctrl=" + (e.Control ? 1 : 0) + " shift=" + (e.Shift ? 1 : 0) + " alt=" + (e.Alt ? 1 : 0));
        };
        box.KeyUp += delegate (object s, KeyEventArgs e) { Say("up " + e.KeyCode); };
        box.TextChanged += delegate { Say("text " + Uri.EscapeDataString(box.Text)); };
        form.Shown += delegate
        {
            Point at = pattern.PointToScreen(Point.Empty);
            Say("ready " + at.X + " " + at.Y + " " + side + " " + side);
        };
        form.FormClosed += delegate { Say("bye"); };
        Timer limit = new Timer();
        limit.Interval = seconds * 1000;
        limit.Tick += delegate { form.Close(); };
        limit.Start();
        Application.AddMessageFilter(new Wheel());
        Application.Run(form);
    }
}
'@

Add-Type -TypeDefinition $source -ReferencedAssemblies System.Windows.Forms, System.Drawing
[Probe]::Run($X, $Y, $Seconds)
