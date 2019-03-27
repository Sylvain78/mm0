module Main (main) where

import System.IO
import System.Exit
import System.Environment
import qualified Data.ByteString.Lazy as B
import Parser
import AST
import Util
import Elaborator
import Verifier
import ProofTextParser
import FromMM (fromMM)

main :: IO ()
main = do
  getArgs >>= \case
    "verify" : rest -> doVerify rest
    "from-mm" : rest -> fromMM rest
    _ -> die ("incorrect args; use\n" ++
      "  mm0-hs verify MM0-FILE MMU-FILE\n" ++
      "  mm0-hs from-mm MM-FILE [-o MM0-FILE MMU-FILE]\n")

doVerify :: [String] -> IO ()
doVerify args = do
  (mm0, rest) <- case args of
    [] -> return (stdin, [])
    (mm0:r) -> (\h -> (h, r)) <$> openFile mm0 ReadMode
  s <- B.hGetContents mm0
  ast <- either die pure (parse s)
  env <- liftIO (elabAST ast)
  putStrLn "spec checked"
  case rest of
    [] -> die "error: no proof file"
    (mmp:_) -> do
      pf <- readFile mmp
      pf <- liftIO (fromJustError "mmu parse failure" (parseProof pf))
      out <- liftIO (verify s env pf)
      putStrLn "verified"
      mapM_ putStrLn out
